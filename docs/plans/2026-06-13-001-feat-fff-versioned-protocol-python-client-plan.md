---
section: planning
type: plan
status: ready
plan_type: feat
date: 2026-06-13
origin: SPEC-fff-client.md
deciders: Abhijit Salvi
tags: [fff-ipc, protocol, versioning, python-client, dual-read, memory-kit]
---

# feat: Versioned JSON wire protocol + deps-light Python client (`fff-client`)

**Target repo:** `~/my-workspace/util/fff` (this repo). All paths repo-relative.

**Origin:** `SPEC-fff-client.md` (repo root). This plan is the HOW for that spec's WHAT.

---

## Summary

Give an external Python consumer (memory-kit's future `FffAdapter`) a **thin, stdlib-only, version-bounded** way to call fff's **engine daemon** — the warm, shared, frecency-ranked surface the MCP tools use — without depending on the fff Rust toolchain, and **failing loud on protocol skew**.

Two coupled parts:

1. **fff publishes a versioned, language-neutral wire contract.** A self-describing **JSON envelope** carrying `protocol_version` is accepted on the engine sockets *alongside* the existing bincode frames (dual-read), distinguished by a first-byte content sniff. `fff_ipc::PROTOCOL_VERSION` is the single source of truth; the contract is documented in `crates/fff-ipc/PROTOCOL.md`. `fff-mcp`'s `EngineClient` is migrated onto the shared reference implementation (dogfood).
2. **fff ships `clients/python/fff_client`** — stdlib-only (`socket`, `json`, `struct`, `pathlib`), `requires-python >= 3.9` — implementing the documented protocol: socket-path resolution, two-phase handshake with the version check (refuse-on-mismatch), framing, and the consumer verbs.

Closed owner decisions (this session): `list_roots` is a **first-class wire verb**; the Python client is **connect-and-fail** (no daemon spawn); `record_access` is **shipped but opt-in, default off**.

---

## Problem Frame

memory-kit (Python) is building a `CompositeBackend` that fuses fff (lexical/frecency) with turbo-rag (semantic) behind one `RetrievalBackend` port (memory-kit ADR-022). For the fff leg it needs the **same rankings the MCP tools return**, which only the **socket daemon** provides (the `fff-c` FFI cdylib is a separate in-process surface with its own cold index and private frecency — explicitly not the target).

Today a Python consumer has no first-class path to that daemon. The three available routes are all the coupling we are removing: (a) hand-roll bincode + the two-phase handshake against an **unversioned** protocol; (b) load the FFI cdylib and lose the warm shared index/frecency; (c) shell out to a non-existent search CLI.

The blocking defect: **the IPC handshake carries no version field**, and the wire is bincode encoded **by variant ordinal**. The only compatibility mechanism is the "appended last" social discipline in the type comments. A skewed client and engine **do not fail loud — they silently misinterpret bytes**. Any external client we publish is exposed to silent corruption on a brew upgrade or a stale daemon unless we add an explicit, refuse-on-mismatch version handshake. turbo-rag already does this (`schema_version` + `SCHEMA_MISMATCH`); fff must gain the equivalent.

---

## Requirements

- **R1** — An external Python process can run `grep`, `find_files`, `multi_grep`, and `list_roots` against the warm engine daemon and receive the same frecency-ranked, git-status-annotated results the MCP tools return.
- **R2** — The wire payload for external consumers is **language-neutral JSON** (not hand-rolled bincode), carrying an explicit `protocol_version`.
- **R3** — Version skew **fails loud**: an incompatible `protocol_version` yields a typed `PROTOCOL_MISMATCH` error and **never** a partial/garbled decode.
- **R4** — `PROTOCOL_VERSION` is single-sourced in `fff-ipc` and spoken by the engine, the migrated `fff-mcp` `EngineClient`, and the Python client.
- **R5** — **Migration safety (dual-read):** a new engine continues to serve existing legacy bincode clients (editor, `fffctl`, un-migrated `fff-mcp`) unchanged until they age out. The legacy path is not removed in this increment.
- **R6** — The Python client is **stdlib-only**, `requires-python >= 3.9`, pure-Python, no compiled artifact, no `fff` Rust artifact in the consumer's dependency tree.
- **R7** — The protocol contract is documented in `crates/fff-ipc/PROTOCOL.md`, and the decision is recorded as `docs/adr/ADR-002-*.md`.
- **R8** — The two consumption surfaces stay distinct: this work touches only the daemon socket; the `fff-c`/JS/Bun in-process surface is untouched.

---

## High-Level Technical Design

### Dual-read dispatch (the core mechanism)

The codec frames every message as `[u32-LE len][payload]` with **no type tag**. Rather than change the framing, the engine sniffs the **first byte of the decoded payload** to route to the right decoder. Legacy bincode first-frames always begin with a small variant ordinal; the JSON envelope always begins with `{`.

```
            recv frame  [u32-LE len][payload bytes]
                              │
                       peek payload[0]
                    ┌─────────┴───────────┐
              0x7B '{'                  else (0x00–0x0A …)
                    │                       │
         JSON envelope path          legacy bincode path
         (versioned, new)            (UNCHANGED — R5)
                    │                       │
        parse {protocol_version,    bincode::deserialize
         verb, params}                 MasterRequest /
                    │                  SearchRequest
        version check ──mismatch──▶ JSON error
              │  match               PROTOCOL_MISMATCH
        handle verb → Wire* result      (loud, R3)
        → serialize JSON response
```

**Why the sniff is unambiguous:** bincode encodes the leading enum variant index as a little-endian `u32`, so the first payload byte equals the variant ordinal. The reachable first-message variants are `MasterRequest` 0–6 and `SearchRequest::{Connect=8, Health=9, DropRoot=10}` — every one `≤ 0x0A`. `0x7B` (`{`) can never be a legacy first byte, so the test is collision-free. This is documented as a hard invariant in `PROTOCOL.md`.

### Two-phase handshake, versioned (JSON path)

```
client                         master.sock                 worker-N.sock
  │  {pv, verb:"handshake",        │                            │
  │   base_path} ────────────────▶ │  version check (R3)        │
  │ ◀── {worker_socket, idx} ──────│                            │
  │                                                             │
  │  {pv, verb:"connect", base_path} ─────────────────────────▶ │ version check
  │ ◀──────────────────────────────────── {ok:true, "ack"} ─────│
  │  {pv, verb:"grep", params:{…}} ───────────────────────────▶ │
  │ ◀──────────────────── {ok:true, result: WireGrepResponse} ──│
```

`protocol_version` is validated at **both** the master handshake and the worker connect (a client could reach a worker whose engine differs only in theory, but validating both keeps each socket self-defending). Mismatch → `{ok:false, error:{code:"PROTOCOL_MISMATCH", message, engine_version, client_version}}`, then close.

### Envelope schema (directional)

Request: `{"protocol_version": <u32>, "verb": "<name>", "params": { … }}`
Response (ok): `{"protocol_version": <u32>, "ok": true, "result": <verb-specific JSON>}`
Response (err): `{"protocol_version": <u32>, "ok": false, "error": {"code": "<CODE>", "message": "<text>", …}}`

`result` payloads are the **serde-JSON of the existing `Wire*` structs** (`WireGrepResponse`, `WireSearchResult`, `WireGitFile`, `WireDirEntry`, `HealthResponse`) — no new result shapes, just JSON instead of bincode. `record_access` returns no frame (fire-and-forget), matching the bincode behavior.

### `list_roots` reconciliation

The engine master knows **loaded** roots (`RoutingTable` base_paths); the `[mcp]` config (already parsed into `FffConfig.mcp` that the engine loads) knows **configured** roots with `name`/`default`. The `list_roots` verb returns the **union**: configured roots (with `name`, `default`) plus any live-but-unconfigured base_paths (`name: null`, `default: false`).

---

## Output Structure

New top-level consumer tree (greenfield):

```
clients/
  python/
    pyproject.toml            # requires-python >=3.9, stdlib-only, no deps
    README.md                 # compatibility statement (client ↔ PROTOCOL_VERSION ↔ engine range)
    src/
      fff_client/
        __init__.py           # FffClient, FffError, FffUnreachable, default_socket_paths, PROTOCOL_VERSION
        _protocol.py          # envelope encode/decode, framing, sniff, version check
        _paths.py             # XDG socket-path resolution mirroring fff_ipc::paths
        _client.py            # FffClient: two-phase handshake, verbs, context manager
    tests/
      test_import_minimal.py
      test_fake_engine.py     # round-trip + version-skew against an in-process fake
      test_paths_parity.py
      test_live_engine.py     # integration, gated on a real fff-engine binary
```

`crates/fff-ipc/PROTOCOL.md` and `docs/adr/ADR-002-versioned-protocol-and-client.md` are added in-tree.

---

## Key Technical Decisions

- **KTD1 — Dual-read by first-byte content sniff (`{` ⇒ JSON, else bincode).** The frame carries no discriminator and adding one would change the framing for legacy clients. Sniffing `payload[0]` is zero-framing-change, leaves the bincode path byte-identical, and is collision-free (legacy first bytes ≤ 0x0A). *Alternatives:* a second socket path (`master-v2.sock`) — doubles path/lifecycle management and spawn logic; a leading magic/version byte — changes the frame for everyone, breaking R5.
- **KTD2 — The version rides in the JSON envelope, not a bincode field.** bincode is not self-describing and has no field-skip; adding `protocol_version` to `MasterRequest::Handshake` is itself a breaking wire change. The envelope is name-tagged (`verb`), so it is reorder-resilient where bincode is ordinal-fragile.
- **KTD3 — Single `PROTOCOL_VERSION` (u32) in `fff-ipc`.** Spoken by engine, migrated `EngineClient`, and the Python client. Bump policy documented in `PROTOCOL.md`.
- **KTD4 — Refuse-on-mismatch, loud (R3).** No best-effort decode. Engine returns `PROTOCOL_MISMATCH` with both versions; client raises `FffError(code="PROTOCOL_MISMATCH")`.
- **KTD5 — `list_roots` is a first-class wire verb** (owner decision), sourced from configured `[mcp]` roots ∪ live routing-table base_paths.
- **KTD6 — Python client is connect-and-fail** (owner decision): resolves the socket and connects; raises `FffUnreachable` if the daemon is absent. No `fff-engine` spawn, no binary-on-PATH requirement.
- **KTD7 — `record_access` shipped but opt-in, default off** (owner decision): exposed on the client surface, gated behind an explicit flag defaulting to off, so agent retrieval does not pollute the human frecency signal unless opted in.
- **KTD8 — Socket-path resolution parity is a correctness contract.** The Python `default_socket_paths()` mirrors `fff_ipc::paths` precedence **exactly** (`$XDG_CACHE_HOME` → `$HOME/.cache` → platform cache dir → `/tmp`, then `fff/master.sock`) and accepts an explicit override (param + `FFF_MASTER_SOCK` env). A parity test pins it. The client never needs the blake3 slug — it connects to `master.sock` (fixed) and then to the worker path the handshake returns.
- **KTD9 — Bounded frame length on the JSON read path.** The existing codec trusts the 4-byte length with no guard; the new JSON read path adds a max-frame guard (prior art: the truncation-hardening plans) so a hostile/garbled length can't trigger an unbounded allocation.

---

## Implementation Units

### U1. Protocol foundation in `fff-ipc` — version, envelope, JSON codec, sniff

**Goal:** Establish the versioned JSON envelope as a reference implementation in the protocol crate.
**Requirements:** R2, R3, R4, KTD1, KTD2, KTD3, KTD9.
**Dependencies:** none.
**Files:**
- `crates/fff-ipc/src/protocol.rs` (new) — `PROTOCOL_VERSION: u32`, envelope request/response types, `verb` tagging, `encode_json`/`decode_json` over the existing `[u32-LE len][payload]` frame, a `looks_like_json(payload: &[u8]) -> bool` sniff helper, a max-frame guard constant, and `PROTOCOL_MISMATCH`/error-code definitions.
- `crates/fff-ipc/src/lib.rs` (modify) — add `pub mod protocol;` and re-export `PROTOCOL_VERSION` + envelope types.
- `crates/fff-ipc/src/codec.rs` (modify) — add a JSON-payload read/write entry point (or a generic "read framed bytes" + decode split) so the JSON path reuses the framing without forcing bincode; add the length guard to the new path.
- `crates/fff-ipc/src/lib.rs` `IpcError` (modify) — add JSON encode/decode error variants (do not disturb the bincode variants).
**Approach:** Reuse the existing `Wire*`/options structs verbatim as the JSON result payloads (serde already derives JSON). The envelope is a thin tagged wrapper; verbs map 1:1 to existing `SearchRequest`/`MasterRequest` semantics. Keep the bincode functions untouched.
**Patterns to follow:** `crates/fff-ipc/src/codec.rs` framing; `crates/fff-ipc/src/types.rs` serde-derive structs; `routing.rs` for existing `serde_json` usage.
**Test scenarios:**
- Envelope round-trips: encode a `grep` request envelope → decode → fields preserved.
- `looks_like_json` returns true for a `{`-leading payload and false for bincode first-bytes `0x00`–`0x0A` (table test across the reachable variant ordinals). Covers KTD1.
- A frame whose declared length exceeds the guard is rejected with a typed error, not an allocation. Covers KTD9.
- `PROTOCOL_VERSION` is a single exported constant; no second definition exists in the crate.
**Verification:** `cargo test -p fff-ipc` passes; the crate exposes `protocol::PROTOCOL_VERSION` and envelope codec entry points.

### U2. Engine master — dual-read dispatch + version handshake

**Goal:** The master accepts JSON-envelope requests alongside legacy bincode and enforces the version check.
**Requirements:** R1, R3, R5, KTD1, KTD4.
**Dependencies:** U1.
**Files:** `crates/fff-engine/src/master.rs` (modify `handle_connection` read site ~L800–893).
**Approach:** At the master read site, read the framed payload bytes once, branch on `looks_like_json`. JSON branch: parse envelope → if `protocol_version` incompatible, write a `PROTOCOL_MISMATCH` JSON error and close (loud); else map `verb` to the existing handling (`handshake` → existing route/assign logic returning `{worker_socket, idx}` as JSON; `health`, `route_info`, etc.). Legacy branch: feed the bytes to the unchanged bincode `MasterRequest` path. Response encoding mirrors the request encoding.
**Execution note:** Characterization-first — capture the current legacy `Handshake`/`ListWorkers`/`Health` round-trips before refactoring the read site, so dual-read cannot regress them.
**Patterns to follow:** existing `MasterRequest` match arms; `worker_socket_path` validation in `master_handshake`.
**Test scenarios:**
- JSON handshake with the current `PROTOCOL_VERSION` returns a JSON `{worker_socket, worker_index}`.
- JSON handshake with an incompatible version returns `PROTOCOL_MISMATCH` (both versions present) and no worker socket. Covers R3.
- A legacy bincode `Handshake` still returns a bincode `WorkerSocket` (regression). Covers R5.
- Interleaved: a legacy client and a JSON client handshake against the same master in one test get correct, non-cross-contaminated responses.
**Verification:** `cargo test -p fff-engine` passes incl. new dual-read tests; existing master tests unchanged.

### U3. Engine worker — dual-read for connect + search verbs

**Goal:** Workers accept the JSON `connect` + verb frames alongside legacy bincode, validating the version, and reuse the existing dispatch.
**Requirements:** R1, R3, R5, KTD1, KTD4.
**Dependencies:** U1.
**Files:** `crates/fff-engine/src/worker.rs` (modify first-message read ~L425 and per-request loop ~L468); `crates/fff-engine/src/server.rs` (`dispatch_request` ~L131 — reuse as-is, mapping JSON verbs to the same handlers).
**Approach:** Sniff at both read sites. JSON first message must be `connect` (version-checked) or `health`; subsequent JSON verbs (`grep`/`find_files`/`multi_grep`/`list_recent_files`/`get_git_status`/`list_directories`/`record_access`) deserialize params into the existing options structs and call the existing handlers, then serialize the `Wire*` result as a JSON response. `record_access` writes no response (parity with bincode). Legacy bincode path unchanged.
**Patterns to follow:** `dispatch_request` handler mapping; the existing first-message `Connect`/`Health`/`DropRoot` discrimination.
**Test scenarios:**
- JSON `connect` + `grep` returns a `WireGrepResponse`-shaped JSON `result` with `frecency_score` populated.
- JSON `record_access` writes and sends **no** reply (client must not block). Covers fire-and-forget parity.
- JSON verb with an incompatible version on the worker connect → `PROTOCOL_MISMATCH`.
- Legacy bincode `Connect` + `Grep` still round-trips (regression). Covers R5.
- A non-`connect`/`health` JSON first message is rejected and the connection closed (mirrors legacy first-message discipline).
**Verification:** `cargo test -p fff-engine` passes; both encodings answered on the same worker.

### U4. `list_roots` wire verb

**Goal:** A non-MCP consumer can enumerate targetable roots over the protocol.
**Requirements:** R1, KTD5.
**Dependencies:** U2.
**Files:** `crates/fff-engine/src/master.rs` (add the `list_roots` JSON verb handler); `crates/fff-ipc/src/protocol.rs` (the verb + result type `{base_path, name?, default}`).
**Approach:** Source the configured `[mcp]` roots from the engine's loaded `FffConfig.mcp` (`name`, `default`) and union with live routing-table base_paths (`name: null`, `default: false`); de-dup by canonical path, default-first ordering (mirrors `RootRegistry::all_with_names`). JSON-only verb (no bincode equivalent needed — legacy clients use the MCP registry).
**Patterns to follow:** `crates/fff-mcp/src/registry.rs` `all_with_names`; master `collect_worker_info`.
**Test scenarios:**
- With two configured roots + one live-only base_path, `list_roots` returns all three, default-first, configured ones carrying `name`/`default`, the live-only one `name:null`.
- De-dup: a configured root that is also live appears once.
- Empty config + one live root → single entry, `default:false`, `name:null`.
**Verification:** `cargo test -p fff-engine` passes; verb returns the reconciled union.

### U5. Dogfood — migrate `fff-mcp` `EngineClient` to the shared JSON reference client

**Goal:** `fff-mcp` speaks the documented protocol via the `fff-ipc` reference implementation — one wire format, no second copy.
**Requirements:** R4, R7 (dogfood), and the spec's `fff-mcp` regression test.
**Dependencies:** U1, U2, U3, U4.
**Files:** `crates/fff-mcp/src/client.rs` (rewrite `EngineClient` to encode/decode the JSON envelope via `fff-ipc::protocol`, preserving the public method surface, `SPAWN_TIMEOUT`/`HANDSHAKE_READ_TIMEOUT`/`QUERY_READ_TIMEOUT`, `ensure_master_running`, `search_with_recovery`, and `record_access` fire-and-forget); `crates/fff-mcp/src/server.rs` (no tool-surface change — only the underlying call path moves).
**Approach:** Keep `EngineClient`'s method signatures stable (server.rs proxy fns unchanged). Swap the body to send versioned JSON envelopes and parse JSON responses. The version handshake now runs on connect; a `PROTOCOL_MISMATCH` surfaces as the existing error channel. `fffctl` is **not** migrated this increment — it keeps using legacy bincode `MasterRequest`, which the dual-read engine still serves (deferred; see Scope Boundaries).
**Execution note:** Lean on the existing `fff-mcp` test suite as the regression gate before/after the swap.
**Test scenarios:**
- `find_files`/`grep`/`multi_grep` through `EngineClient` return the same typed results as before the migration (regression against existing fff-mcp tests).
- `record_access` still sends without awaiting a reply.
- `EngineClient::connect` against a version-incompatible engine surfaces a `PROTOCOL_MISMATCH` error rather than hanging or garbling.
- `check_health` path still works (may stay on the read-only `RouteInfo` route; note if it remains bincode).
**Verification:** existing `crates/fff-mcp` tests pass unchanged; MCP tool behavior identical from Claude Code's view.

### U6. `clients/python/fff_client` — package + client implementation

**Goal:** A stdlib-only Python client implementing the documented protocol.
**Requirements:** R1, R2, R3, R6, KTD6, KTD7, KTD8.
**Dependencies:** U1 (envelope shape), and ideally U2–U4 for live testing.
**Files:** `clients/python/pyproject.toml`, `clients/python/README.md`, `clients/python/src/fff_client/__init__.py`, `_protocol.py`, `_paths.py`, `_client.py` (all new).
**Approach:** `_paths.default_socket_paths()` mirrors `fff_ipc::paths` precedence exactly and honors an override (param + `FFF_MASTER_SOCK`). `_protocol` implements `[u32-LE len][json]` framing (`struct`), envelope build/parse, and the version check. `FffClient(base_path=…)` does the two-phase handshake (`PROTOCOL_MISMATCH` → `FffError`; unreachable socket → `FffUnreachable`; **no spawn**), binds to one root, and is a context manager. Verbs: `grep`, `find_files`, `multi_grep`, `list_roots` (core); `list_recent_files`, `get_git_status`, `health` (useful); `record_access` present but gated behind an opt-in flag, default off (no reply read). Pin `PROTOCOL_VERSION` to match `fff-ipc`. README states the client↔protocol↔engine compatibility matrix.
**Patterns to follow:** the spec's proposed Python surface; turbo-rag-client's `>=3.9` stdlib shape (sibling spec).
**Test scenarios:** (see U7 for the running tests)
- Public surface exports `FffClient`, `FffError`, `FffUnreachable`, `default_socket_paths`, `PROTOCOL_VERSION`.
- `record_access` is a no-op unless the opt-in flag is set; when set, it sends and does not block on a reply. Covers KTD7.
- Connecting with no daemon present raises `FffUnreachable` (not a hang, not a generic socket error). Covers KTD6.
**Verification:** `python3.9 -c "import fff_client"` succeeds in a bare environment (no third-party deps installed).

### U7. Cross-language + integration test matrix

**Goal:** Lock the contract end-to-end and pin the parity/skew invariants the spec enumerates.
**Requirements:** R1, R3, R5, R6, KTD8.
**Dependencies:** U6 (and U2–U5 for live/dual-read).
**Files:** `clients/python/tests/test_import_minimal.py`, `test_fake_engine.py`, `test_paths_parity.py`, `test_live_engine.py` (new); a small Rust dual-read regression test lives with U2/U3.
**Approach:** A fake `socketserver` Unix-socket engine speaking the documented protocol drives fast, hermetic tests; a gated live test runs against a real `fff-engine` over a temp repo.
**Test scenarios:**
- Import under 3.9 with nothing heavy installed. Covers R6.
- Fake-engine round-trip: two-phase handshake completes; `find_files`/`grep` resolve to typed results; `record_access` (opt-in) writes without awaiting a reply.
- Version-skew: fake server advertises an incompatible `protocol_version` → client raises `FffError(PROTOCOL_MISMATCH)` and returns **no** partial results. Covers R3.
- Path parity: `default_socket_paths()` resolves the same `master.sock` path as the engine for a matrix of `$XDG_CACHE_HOME`/`$HOME` settings. Covers KTD8.
- Live integration: against a real engine, `find_files`/`grep` return expected paths, `frecency_score` is populated, and results match the MCP tools for the same query.
- Engine dual-read regression: the engine answers both a legacy bincode frame and a JSON frame in the same test window. Covers R5.
**Verification:** `pytest clients/python` green (live test skipped when no engine binary); Rust dual-read tests green.

### U8. `PROTOCOL.md` + ADR-002

**Goal:** Publish the stable contract and record the decision.
**Requirements:** R7.
**Dependencies:** U1–U5 (the wire shape must be settled).
**Files:** `crates/fff-ipc/PROTOCOL.md` (new); `docs/adr/ADR-002-versioned-protocol-and-client.md` (new).
**Approach:** `PROTOCOL.md` documents framing, the first-byte sniff invariant, the two-phase handshake with `protocol_version`, the per-verb JSON request/response schemas (referencing the `Wire*` shapes), the error codes (incl. `PROTOCOL_MISMATCH`), the `PROTOCOL_VERSION` bump policy, and the **legacy bincode variant-append discipline documented as the superseded rule**. ADR-002 follows ADR-001 house style (YAML frontmatter; Context / Decision / Consequences / Alternatives Considered) covering the JSON-envelope + version handshake + dual-read migration + the three owner decisions.
**Test expectation:** none — documentation. Verified by review against the implemented wire shape.
**Verification:** `PROTOCOL.md` matches the implemented envelope; ADR-002 renders and matches house style.

---

## Scope Boundaries

**In scope:** the daemon-socket JSON protocol, version handshake, dual-read, `list_roots` verb, `fff-mcp` `EngineClient` migration, the Python client + tests, `PROTOCOL.md`, ADR-002.

**Out of scope (from the spec):** fff search semantics (ranking/frecency/query parsing/glob); the MCP tool argument shapes; the `fff-c`/JS/Bun in-process surface and its `FFF_CREATE_OPTIONS_VERSION`; the master/worker sharding model; vendoring a client inside memory-kit.

### Deferred to Follow-Up Work
- **Migrate `fffctl` to the JSON protocol.** It keeps using legacy bincode `MasterRequest` this increment; the dual-read engine serves it unchanged. Migrate once the JSON path is proven, then plan legacy-path removal.
- **Remove the legacy bincode path.** Only after all in-flight clients (editor, `fffctl`, `fff-mcp`) have aged onto JSON — a separate increment.
- **memory-kit `FffAdapter` wiring** (consumer side, memory-kit repo / ADR-022).
- **Publishing `fff-client` to PyPI** — in-repo under `clients/python/` for now.

---

## Risks & Dependencies

- **Socket-path resolution drift (R6/KTD8).** If Python's resolution diverges from `fff_ipc::paths`, the client silently can't find the daemon. *Mitigation:* exact-mirror + explicit override + the parity test (U7).
- **Version skew against an *old* engine with no handshake.** A new JSON client hitting an old (pre-this-work) engine sends `{…}`; the old engine bincode-decodes it and almost certainly errors/drops rather than returning garbage, but it won't emit a clean `PROTOCOL_MISMATCH`. *Mitigation:* document in `PROTOCOL.md` that `PROTOCOL_MISMATCH` requires an engine new enough to sniff; ship engine + client together; the connect timeout bounds the failure.
- **Unbounded frame length (existing).** Inherited from the codec. *Mitigation:* KTD9 guard on the new JSON read path.
- **bincode ordinal fragility during the transition.** Both encodings share the same Rust enums; the "appended last" discipline still governs the bincode side until it's removed. *Mitigation:* call it out in `PROTOCOL.md`; no reordering during dual-read.
- **Dependency ordering:** U1 gates everything; U2/U3 are parallelizable; U4 needs U2; U5 needs U1–U4; U6 needs U1 (and U2–U4 for live tests); U7 needs U6; U8 last.

---

## Alternatives Considered

- **Hand-roll bincode in Python.** Re-derives an undocumented, ordinal-fragile format with a silent-skew failure mode. Rejected.
- **Rust client crate + `cdylib`/`pyo3` shim.** Keeps bincode internal but forces a per-platform compiled artifact on the consumer — the exact toolchain coupling being removed. Rejected.
- **`fff search` CLI emitting JSON.** Process-spawn-per-query and awkward for `record_access`; fine as a later scripting affordance, not the primary path. Deferred, not chosen.
- **Second versioned socket (`master-v2.sock`) instead of content-sniff.** Avoids sniffing but doubles socket lifecycle, path resolution, and spawn logic. Rejected in favor of KTD1.

---

## Sources & Research

- `SPEC-fff-client.md` (origin).
- Codebase surface map (this session): `crates/fff-ipc/src/{types,codec,paths,lib,config}.rs`, `crates/fff-mcp/src/{client,server,registry}.rs`, `crates/fff-engine/src/{master,worker,server}.rs`, `docs/adr/ADR-001-engine-worker-model.md`. Confirmed: bincode-only frame with no discriminator; `serde_json` already a `fff-ipc` dependency; no `PROTOCOL_VERSION` exists; no `clients/` dir; `list_roots` already emitted as hand-built JSON in the MCP layer (precedent).
- turbo-rag client spec — `~/my-workspace/ai/turbovec/SPEC-turbo-rag-client-extraction.md` (the `schema_version` + refuse-on-mismatch model).
- memory-kit ADR-022 (consumer side).
