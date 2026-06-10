---
status: active
type: feat
depth: standard
created: 2026-06-10
---

# feat: Multi-Root fff-mcp

## Summary

`fff-mcp` becomes multi-root: tools accept an optional `base_path` argument, the server pre-registers additional roots via a repeatable `--root` flag, and a new `list_roots` MCP tool lets AI consumers discover registered roots. A per-session connection pool reuses one `EngineClient` per canonicalized base_path. Multi-root tool calls require master mode; the default (no `base_path`) preserves single-root BAU exactly.

## Problem Frame

Today, `fff-mcp` is spawned with `--base-path X` and binds all tool calls to that one root. Agents working from one vault but reasoning across multiple code repos either fall back to system grep (losing fff's frecency-aware ranking) or run multiple `fff-mcp` instances. The master+worker engine already multiplexes — `master::assign_new_root` handles any base_path that hashes to a new slug. The gap is purely in fff-mcp's client: it makes one Connect at startup and never re-handshakes.

This work closes that gap while keeping single-root deployments byte-identical.

---

## Requirements

| R-ID | Requirement |
|------|-------------|
| R1 | Tools `find_files`, `grep`, `multi_grep`, `list_directories`, `list_recent_files`, `get_git_status` accept optional `base_path` argument. |
| R2 | Tool `record_access` resolves the appropriate registered root from its `path` argument (longest-prefix match), falling back to the default root if no match. |
| R3 | New `list_roots` tool returns the list of registered roots with `default: bool`. |
| R4 | New repeatable `--root <PATH>` clap flag at startup; each `--root` adds an entry to the registry. `--base-path X` continues to set the default and is auto-added to the registry. |
| R5 | fff-mcp maintains a connection pool keyed on canonicalized base_path; first call for a path does Handshake → Connect, subsequent calls reuse the cached `EngineClient`. |
| R6 | When the caller supplies a `base_path` that is **not** the default and master mode is unavailable, the tool call returns a clear error (`"multi-root requires master mode"`). The default root continues to work in either mode. |
| R7 | All existing tool calls (no `base_path` argument supplied) continue to work unchanged — no schema-required arguments added, no behavior change for single-root deployments. |
| R8 | Unregistered `base_path` values are accepted (master decides routing) — registry is a discoverability hint, not an access gate. |

## Success Criteria

- Existing `fff-mcp` integration tests pass without modification.
- A new integration test starts a master, registers two roots, and verifies tool calls with explicit `base_path` route to the right worker.
- `list_roots` returns the expected shape.
- Existing single-root MCP wrappers (Claude Code's, etc.) keep working without any config change.

---

## Key Technical Decisions

### KTD1. Connection pool keyed on canonicalized base_path

Each unique base_path gets its own `EngineClient`. Canonicalization mirrors `fff_ipc::paths::base_path_slug` (canonicalize → fallback to raw input) so the pool key matches what the master sees on Handshake. Stored as `HashMap<PathBuf, EngineClient>` under a `Mutex` (or `tokio::sync::Mutex` if the surrounding code is async). No proactive eviction — connections live for the MCP session.

### KTD2. `--root` is additive to `--base-path`

`--base-path` keeps its current meaning: the default for tool calls without explicit `base_path`. `--root` (repeatable) pre-registers additional roots for discovery. The startup registry is `{ default: PathBuf, additional: Vec<PathBuf> }`. `list_roots` exposes both with `default: true` flagged on the one default.

### KTD3. Multi-root requires master mode

Legacy per-root daemons can only serve one base_path. Multi-root mode only makes sense when master is running. If the caller supplies a non-default `base_path` and master is unreachable, return an error rather than spawning legacy daemons on-demand.

### KTD4. Stale connection recovery

If a cached `EngineClient` returns an IPC error (worker died, evicted, or master rerouted), fff-mcp drops the cache entry and retries once with a fresh Handshake. The second Handshake may yield a different worker socket — that's the master making an LRU decision, and the client should honor it.

### KTD5. `record_access` path → root resolution

`record_access(path)` resolves which registered root owns the file:
1. Canonicalize `path`.
2. Find the longest-prefix registered root that contains it.
3. If none match, fall back to the default root.
4. Forward `record_access` to that root's `EngineClient`.

### KTD6. No new IPC variants

The master+worker engine already supports `MasterRequest::Handshake { base_path }`. fff-mcp simply does additional Handshakes — no `MasterRequest`, `SearchRequest`, or response variant needs to change. Wire format and Cargo.lock untouched outside `fff-mcp`.

---

## High-Level Technical Design

```
                           ┌─────────────────────────────────────────────────┐
                           │                  fff-mcp process                │
                           │                                                 │
   MCP tool call ────────► │  tool handler                                   │
   (find_files, etc.,      │      │                                          │
    optional base_path)    │      ▼                                          │
                           │  resolve_base_path                              │
                           │   (arg ?? default ── canonicalize)              │
                           │      │                                          │
                           │      ▼                                          │
                           │  ConnectionPool.get_or_connect(base_path)       │
                           │      │ cache hit ────► reuse EngineClient ──┐   │
                           │      │ cache miss                           │   │
                           │      ▼                                      │   │
                           │  Handshake → WorkerSocket → Connect → Ack   │   │
                           │      │                                      │   │
                           │      ▼                                      │   │
                           │  insert into HashMap<PathBuf, EngineClient> │   │
                           │      │                                      │   │
                           │      └──────────────────────────────────────┘   │
                           │                                                 │
                           │                          ▼                      │
                           │                   issue search query            │
                           │                          │                      │
                           └─────────────────────────────────────────────────┘
                                                      │
                                                      ▼
                                          fff-engine master / worker
```

**Failure mode (KTD4):** if `EngineClient` returns an IPC error on a cached connection, pool drops the entry and retries the `Handshake → Connect → Ack` sequence once. Second failure surfaces to the caller.

---

## Implementation Units

### U1. Connection pool abstraction

**Goal:** Replace the single-EngineClient-at-startup pattern with a pool keyed on canonicalized base_path.

**Requirements:** R5, KTD1, KTD4.
**Dependencies:** none.

**Files:**
- Modify: `crates/fff-mcp/src/client.rs`
- Create: `crates/fff-mcp/src/pool.rs`
- Modify: `crates/fff-mcp/src/lib.rs` (declare new module)
- Test: inline `#[cfg(test)]` unit tests in `pool.rs`

**Approach:**
- New struct `ConnectionPool` owning a `HashMap<PathBuf, Arc<EngineClient>>` behind a `Mutex`.
- `get_or_connect(&self, base_path: &Path)`: canonicalize, check cache, miss → Handshake → Connect → insert → return.
- `invalidate(&self, base_path: &Path)`: drop the cache entry.
- `retry_with_fresh<F>(...)`: once-only retry that invalidates on first failure.

**Test scenarios:** caches per base_path, distinct paths get distinct connections, canonicalization equivalence (./ and absolute), fallback when path missing, invalidate scoped, retry once.

**Verification:** `cargo test -p fff-mcp --no-default-features pool::tests` green.

### U2. Registry: `--root` flag and startup registry

**Goal:** Accept `--root` (repeatable) and build a registry with `default + additional`.

**Requirements:** R4, KTD2.
**Dependencies:** none.

**Files:**
- Modify: `crates/fff-mcp/src/main.rs`, plus the server config struct.
- Test: inline tests.

**Approach:**
- Clap: `#[arg(long, value_name = "PATH")] root: Vec<PathBuf>`.
- `RootRegistry { default: PathBuf, additional: Vec<PathBuf> }`; canonicalize and dedupe on construction.
- `RootRegistry::all()` returns `(path, is_default)` pairs.

**Test scenarios:** default-only when no extras, dedupe extras against default, canonicalize relative paths, `all()` returns default first.

### U3. `list_roots` MCP tool

**Goal:** Expose registered roots to AI consumers.

**Requirements:** R3.
**Dependencies:** U2.

**Files:** modify `crates/fff-mcp/src/server.rs`.

**Approach:** new tool `list_roots`, no args, returns `{"roots": [{"base_path": "...", "default": bool}, ...]}`.

**Test scenarios:** returns registered set, exactly one `default: true`.

### U4. Optional `base_path` argument on search tools

**Goal:** Route tool calls to the right worker based on optional `base_path`.

**Requirements:** R1, R6, R7, R8, KTD1, KTD3.
**Dependencies:** U1, U2.

**Files:** modify each tool handler in `crates/fff-mcp/src/server.rs`; integration test in `crates/fff-mcp/tests/integration.rs`.

**Approach:**
- Add `#[serde(default)] base_path: Option<String>` to each tool's param struct.
- Helper `resolve_base_path` returns arg or registry default.
- Master-mode gate for non-default paths; error message: `"multi-root requires master mode"`.
- Run query via `pool.retry_with_fresh`.

**Test scenarios:** explicit base_path routes correctly, no-arg uses default, non-default without master errors, unregistered base_path still works via master.

### U5. `record_access` path → root resolution

**Goal:** Route `record_access(path)` to the registered root containing `path`.

**Requirements:** R2, KTD5.
**Dependencies:** U1, U2.

**Files:** modify `record_access` handler in `crates/fff-mcp/src/server.rs`.

**Approach:** new `resolve_root_for_path` — canonicalize, longest-prefix match, default fallback.

**Test scenarios:** path under root A routes to A, longest prefix wins, unmatched falls back to default.

### U6. Polish

**Goal:** Clap help text, tool description quality, error message consistency.

**Requirements:** R3, R6.
**Dependencies:** U2, U3, U4.

**Approach:** `--root` help mentions multi-root + master-mode requirement; tool descriptions point to `list_roots`; error string consistent across tools.

---

## Scope Boundaries

### In scope
- All seven existing search tools gain optional `base_path`.
- `record_access` gains automatic path-to-root resolution.
- New `list_roots` tool.
- `--root` clap flag (repeatable).
- In-memory connection pool with stale-connection retry.
- Master-mode requirement enforcement for non-default roots.
- Integration tests covering the multi-worker routing flow.

### Deferred to follow-up work
- Config-file registry at `~/.config/fff/mcp.toml`.
- Spawning legacy per-root daemons on-demand for unregistered base_paths when master is unavailable.
- Cross-MCP-session connection sharing.
- Client-side `base_path` validation against directory existence.

---

## Risks & Dependencies

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Concurrent `get_or_connect` races on same uncached path. | Low | Low | Accept v1 — last-writer-wins, one redundant connection is harmless. |
| `EngineClient` cached after master restart returns stale-socket errors. | Medium | Low | KTD4 retry-with-fresh. |
| Argument-schema breaking change for strict MCP clients. | Low | Medium | All new args optional (`#[serde(default)]`). |
| Path canonicalization differs from engine canonicalization on edge cases. | Low | Medium | Same `fs::canonicalize` + raw-input fallback as `base_path_slug`. |

---

## System-Wide Impact

- **fff-engine master/worker** — no code change. New Handshake calls fff-mcp issues are already supported by `assign_new_root`.
- **fff-ipc** — no IPC variant additions.
- **fff-ctl** — no change.
- **fff-nvim** — unaffected.
- **CLI/wire compat** — fully additive.

---

## Sources & Research

- `crates/fff-mcp/src/client.rs` — existing `EngineClient` construction.
- `crates/fff-mcp/src/server.rs` — MCP tool registration site.
- `crates/fff-mcp/src/recovery.rs` — existing reconnect contract.
- `crates/fff-ipc/src/paths.rs:43-48` — canonicalization logic to mirror.
- `crates/fff-engine/src/master.rs:231` — `assign_new_root` (receives fff-mcp's new Handshakes).
- `docs/plans/2026-06-09-root-entry-refactor.md` — sibling RootEntry plan.

---

## Open Questions

None blocking. Two known unknowns to verify during implementation:
- Exact module structure inside `crates/fff-mcp/src/` (where `pool.rs` lives).
- Whether `EngineClient` uses `tokio::sync::Mutex` or `std::sync::Mutex`.
