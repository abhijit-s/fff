# SPEC — A thin, deps-light consumer client for the fff engine

> **TARGET REPO: `~/my-workspace/util/fff` (this repo).** This is a design doc,
> not an implementation. No fff source changes are proposed here — it is staged
> so a running fff session can pick it up and decide.
>
> **CONSUMER: memory-kit's future `FffAdapter`.** memory-kit (Python) is building
> a `CompositeBackend` that fuses **fff (lexical / frecency)** with **turbo-rag
> (semantic)** behind one `RetrievalBackend` port (memory-kit ADR-022),
> Reciprocal-Rank-Fusion-merged. fff is the lexical leg; turbo-rag is the
> semantic leg. For the `FffAdapter` to stay light and portable it needs a clean
> way to call fff **without depending on the whole fff Rust build/toolchain**,
> over a **versioned protocol boundary** that fails loud on skew.
>
> This is the fff-side sibling of
> `~/my-workspace/ai/turbovec/SPEC-turbo-rag-client-extraction.md`. The shared
> principle: **the producer of a protocol owns and publishes a thin,
> dependency-light client so consumers don't couple to the whole server.**

---

## The cross-language reality (read this first — it is the whole difference)

The turbo-rag spec could recommend *extracting a Python package* because turbo-rag
**and** its consumer (memory-kit) are both Python — a shared-library import was on
the table. **fff is Rust. memory-kit is Python.** A shared-library *import* is not
possible. So the deliverable is fundamentally different in shape: the stable thing
fff must publish is a **documented, versioned wire/ABI contract**, and the client
is a *separate Python implementation of that contract* (or a binding to a C ABI),
not an extraction of existing fff code.

Everything below is built around that constraint.

---

## What fff actually is (from investigation — not guessed)

fff is a Cargo **workspace** (`edition = 2024`, version `0.16.2`) of nine crates.
The relevant ones:

| Crate | Role |
| ----- | ---- |
| `fff-core` (`fff-search`) | The search SDK: `FilePicker`, `BigramFilter`, `FrecencyTracker`, query parser. `rlib` + `staticlib` + `cdylib`. |
| `fff-engine` | The **daemon**. A `--master` router process + sharded OS-process **workers**, each owning `FilePicker`/`BigramFilter`/`FrecencyTracker` for a shard of roots. |
| `fff-ipc` | **Shared IPC types, framing codec, path helpers** for `fff-engine ↔ fff-mcp`. This is the protocol crate. |
| `fff-mcp` | The **stdio MCP bridge** for Claude Code. Contains `EngineClient` (the only socket-speaking client today) and the `rmcp` tool surface. |
| `fff-ctl` | Operator CLI (`fffctl`) — list/stop/status/health over the master socket. |
| `fff-c` | A **C FFI `cdylib`** (`fff-c`) exporting `fff_*` functions over the C ABI. In-process; owns its own index. |

So **yes — fff has the same engine + stdio-bridge + Unix-socket split as turbo-rag**:

```
                         ┌─────────────────────────────────────────────┐
   Claude Code  ──stdio──▶  fff-mcp  (rmcp tool surface)                │
                         │     │                                        │
                         │     │  EngineClient (sync, bincode)          │
                         │     ▼                                        │
                         │  master.sock ──Handshake{base_path}──▶ master│
                         │     ▲                  │                     │
                         │     │   WorkerSocket{path, idx}              │
                         │     ▼                                        │
                         │  worker-N.sock ──Connect{base_path}──▶ worker│
                         │                   (owns FilePicker/Frecency) │
                         └─────────────────────────────────────────────┘

   (separate, in-process surface — NOT the socket:)
   any host  ──C ABI──▶  libfff_c.{dylib,so}  (own index, own frecency db)
```

### Two distinct consumption surfaces exist today

This matters for the recommendation, because they are **not interchangeable**:

1. **The socket daemon (`fff-ipc` protocol).** Long-lived, shared across all
   clients of a machine. A worker owns the warm `FilePicker` + `BigramFilter` +
   the **shared frecency database** for a root. This is what gives fff its
   *frecency-ranked, git-dirty-boosted* results — the rankings depend on
   accumulated access history written by every fff client (the editor, the MCP
   bridge, `fffctl`). **This is the surface the fff MCP tool surface uses, and
   the one a retrieval consumer wants** if it wants the same ranking memory-kit
   sees through the MCP tools.
2. **The FFI `cdylib` (`fff-c`).** In-process, synchronous, owns its **own**
   `FilePicker`/frecency DB created via `fff_create_instance_with`. The JS/Bun
   bindings (`packages/fff-node`, `packages/fff-bun`) consume *this*. It does
   **not** share the daemon's warm index or shared frecency — a consumer using
   it pays cold-start indexing and sees only its own access history.

A retrieval consumer that wants *what the MCP tools return* must talk to **surface
1 (the socket)**. That is the gap this spec addresses.

### The wire protocol — what it is, and the critical gap

Located in `crates/fff-ipc/`. Concretely:

- **Transport:** Unix domain sockets. Two-phase: connect to `master.sock`, send
  `MasterRequest::Handshake { base_path }`, receive
  `MasterResponse::WorkerSocket { path, worker_index }`; then connect to that
  worker socket, send `SearchRequest::Connect { base_path }`, await
  `SearchResponse::Ack`; all subsequent search traffic is the direct worker
  connection. (`fff-mcp/src/client.rs`, ADR-001.)
- **Framing:** length-prefixed — `[ 4-byte little-endian u32 payload length ][ payload ]`
  (`fff-ipc/src/codec.rs`, `write_message_sync` / `read_message_sync`).
- **Serialization:** **`bincode`** of `serde`-derived Rust enums
  (`MasterRequest`/`MasterResponse`, `SearchRequest`/`SearchResponse` in
  `fff-ipc/src/types.rs`). **Not JSON-RPC.** This is the single biggest
  contract difference from turbo-rag, which uses JSON-RPC 2.0 over its socket.
- **Socket-path resolution:** `master_socket_path()` =
  `<XDG_CACHE>/fff/master.sock`; workers at
  `<XDG_CACHE>/fff/workers/worker-{index}.sock`; legacy per-root socket at
  `<XDG_CACHE>/fff/sockets/<blake3hex16(canonical_base_path)>.sock`
  (`fff-ipc/src/paths.rs`).
- **Spawn-on-absent:** `EngineClient::connect` spawns `fff-engine --master` via
  an `O_CREAT|O_EXCL` race if the master socket is absent, then waits for it.
- **Recovery:** on a worker socket error the client re-runs the two-phase
  handshake (`search_with_recovery`).

**The gap (the heart of this spec):**

> **There is no version field anywhere in the IPC handshake.** Neither
> `MasterRequest::Handshake` nor `SearchRequest::Connect` carries a
> `schema_version`/`protocol_version`. The *only* compatibility mechanism is the
> social discipline visible in the type comments — *"Appended last to preserve
> bincode variant indices for existing variants."* bincode encodes enum variants
> by **ordinal index**, so the protocol is forward/backward compatible **only**
> as long as new variants are appended and field layouts never change. A skewed
> client and engine **do not fail loud — they silently misinterpret bytes** (a
> reordered variant deserializes as a different message; an added field shifts
> every subsequent byte).

This is the opposite of turbo-rag's model, whose `EngineClient` sends a
`schema_version` and **refuses on mismatch** (`EngineError(-32004)`). The fff
protocol has *no such handshake*. Any external client we publish is exposed to
silent corruption on version skew unless we add one. **Closing that gap is a
non-negotiable part of this work** (see *Protocol + versioning* below).

### Existing clients a consumer could use today

- **`fff-mcp::client::EngineClient`** — the only socket-speaking client. It is a
  **synchronous, blocking** Rust struct **internal to the `fff-mcp` binary
  crate** (not published, not a `[lib]` other crates can depend on as a client).
  It is also tangled with MCP concerns (recovery via `crate::recovery`, spawn
  logic, health checks).
- **`fff-ipc`** — a publishable Rust *library* crate with the codec + types +
  paths. A Rust consumer could depend on `fff-ipc` and reimplement the two-phase
  dance. There is **no published, standalone Rust client crate** that wraps it.
- **`fff-c` FFI + JS/Bun bindings** — in-process, the *other* surface (own index,
  not the daemon). Versioned via `FFF_CREATE_OPTIONS_VERSION` (the one place fff
  *does* version a contract).
- **`fffctl`** — a CLI, but an operator tool (list/stop/status/health), not a
  search client. It emits JSON for `--json` management commands but does **not**
  expose grep/find as a stable CLI-JSON contract.
- **No Python binding of any kind exists.**

**Net:** today a Python consumer like memory-kit has **no first-class way** to
reach the warm daemon. Its only options are (a) shell out to a not-yet-existing
search CLI, (b) reimplement bincode + the two-phase handshake by hand against an
unversioned protocol, or (c) load the FFI cdylib and give up the shared warm
index/frecency. All three are exactly the coupling this spec exists to remove.

---

## Goal

A thin, dependency-light, **version-bounded** way for an external (Python)
consumer to call fff's **engine daemon** — getting the *same frecency-ranked,
git-dirty-boosted* results the fff MCP tools return — **without** depending on
the fff Rust build/toolchain, and **failing loud on protocol skew**.

---

## The plan — a deps-light Python client (`fff-client`)

**Decision (owner, narrowed scope): build a Python client, full stop.** Because
fff is Rust and the consumer (memory-kit) is Python, a shared-library import is
out — so the client is a *separate Python implementation of a documented wire
contract*. And because the current wire payload is **bincode** (no
language-neutral spec; hand-rolling serde enum-ordinal layout in Python is
brittle and re-breaks on every Rust type change), the prerequisite is a
**language-neutral, versioned wire envelope**. So the plan has two coupled parts:

**Part 1 — fff exposes a versioned, language-neutral wire contract.** Add a
self-describing **JSON envelope** to the engine's socket payload carrying an
explicit `protocol_version`, alongside the existing bincode variants during a
**dual-read transition**. Publish:

1. **`crates/fff-ipc/PROTOCOL.md`** — the stable contract: framing
   (`[u32-LE len][payload]`), the two-phase handshake **with `protocol_version`**,
   the request/response JSON schemas for the consumer verbs, and error codes.
2. **`fff-ipc::PROTOCOL_VERSION`** — the single source of truth, spoken by the
   engine *and* `fff-mcp`'s `EngineClient` (dogfood — one wire format, no second
   copy; after this, `fff-mcp` is just another consumer of the documented
   contract).

**Part 2 — fff publishes `fff-client` (Python).** A small package
(`fff_client`), **stdlib-only** (`socket`, `json`, `struct`, `pathlib`),
**`requires-python >= 3.9`**, in-repo under `clients/python/` (keeps protocol +
client + engine versioned together). It implements the documented protocol:
socket-path resolution, the two-phase handshake with the version check
(**refuse-on-mismatch, loud**), framing, and the consumer verbs. memory-kit's
`FffAdapter` imports it directly — no Rust toolchain, no `fff` build artifact in
the consumer's dependency tree.

This gives the consumer the **real fff ranking** (the warm shared daemon +
shared frecency the MCP tools use), a **trivial, stable** Python client (JSON,
not hand-rolled bincode), and **loud-on-skew** versioning — mirroring turbo-rag's
schema-version handshake so both legs of memory-kit's `CompositeBackend` behave
consistently.

> *Alternatives considered and rejected for this increment:* (a) hand-rolling
> bincode in Python — brittle, re-derives an undocumented format, silent-skew
> failure mode; (b) a Rust client crate + `cdylib`/`pyo3` shim — keeps bincode
> internal but costs the consumer a per-platform compiled artifact, the exact
> toolchain coupling we are removing; (c) a `fff search` CLI emitting JSON — fine
> as a later scripting affordance, but process-spawn-per-query and awkward for
> `record_access`, so not the primary path.

---

## The operations / API surface a consumer needs

Drawn from the fff MCP tool surface (`crates/fff-mcp/src/server.rs`) and the IPC
`SearchRequest` variants (`crates/fff-ipc/src/types.rs`). memory-kit's lexical
leg needs the search/find/roots verbs; the rest are useful but optional.

| Verb | IPC request | Returns | memory-kit need |
| ---- | ----------- | ------- | --------------- |
| `grep(query, options)` | `SearchRequest::Grep` | `WireGrepResponse` (file-grouped matches + `frecency_score`, `is_definition`, pagination cursor) | **core** |
| `find_files(query, options)` | `SearchRequest::FindFiles` | `Vec<WireSearchResult>` (path, score, `frecency_score`, git status) | **core** |
| `multi_grep(patterns, constraints, options)` | `SearchRequest::MultiGrep` | `WireGrepResponse` | **core** |
| `list_roots()` | (registry / `MasterRequest::ListWorkers` route info) | `[{ base_path, name?, default }]` | **core** (discover targets) |
| `list_recent_files(limit, dirty_only)` | `SearchRequest::ListRecentFiles` | `Vec<WireSearchResult>` | useful |
| `get_git_status(include_clean)` | `SearchRequest::GetGitStatus` | `Vec<WireGitFile>` | useful |
| `list_directories(limit)` | `SearchRequest::ListDirectories` | `Vec<WireDirEntry>` | optional |
| `record_access(path)` | `SearchRequest::RecordAccess` (fire-and-forget, no reply) | — | optional (feeds frecency) |
| `health()` | `SearchRequest::Health` / `MasterRequest::Health` | `HealthResponse` / `HealthReport` (per-root `indexed_files`, `last_scan_age_sec`, `dirty_count`) | recommended (trust-the-index gate) |

Proposed Python surface:

```python
from fff_client import (
    FffClient,            # FffClient(base_path); connect(), close(); context-manager
    FffError,             # .code, .message  (maps protocol error codes → strings)
    FffUnreachable,       # engine not running / spawn failed
    default_socket_paths, # () -> (master_sock: Path, ...)  XDG-cache resolution
    PROTOCOL_VERSION,     # the client's pinned protocol version
)

with FffClient(base_path="/repo") as fff:
    hits  = fff.find_files("retrieval backend", limit=20)
    greps = fff.grep("RetrievalBackend", page_limit=50)
    roots = fff.list_roots()
```

`FffClient(base_path=...)` performs the two-phase handshake (with version check)
and binds the connection to one root, mirroring `EngineClient::connect`.
`base_path` selection (which root to target) is the consumer's `FffAdapter`
concern; the client just resolves the socket and connects.

---

## Protocol + versioning (the non-negotiable)

Cite the turbo-rag model directly: its `EngineClient` sends `schema_version` in
the handshake and **refuses on mismatch** (`EngineError(-32004)`, error name
`SCHEMA_MISMATCH`). fff must gain the equivalent.

- **Add `protocol_version` to the handshake.** Carry it in
  `MasterRequest::Handshake` (or the new JSON envelope's connect frame). The
  engine compares against its own `fff_ipc::PROTOCOL_VERSION`.
- **Refuse-on-mismatch, loud.** On incompatibility the engine returns a typed
  error (e.g. a JSON-RPC error with a stable `PROTOCOL_MISMATCH` code) and the
  Python client raises `FffError(code=PROTOCOL_MISMATCH)`. **No silent
  best-effort decode.** This is the desired resilience, not a bug — a brew
  upgrade or a daemon left running from an older fff must surface as a clear
  error, not as garbled results.
- **`PROTOCOL_VERSION` lives in `fff-ipc`** — single source of truth shared by
  engine, `fff-mcp`'s `EngineClient`, and the published `fff-client`. The Python
  client pins a `PROTOCOL_VERSION` matched to a documented engine range.
- **Compatibility statement** in `clients/python/README.md`: which
  `fff-client` versions speak which `PROTOCOL_VERSION`(s), and the engine
  version ranges those map to.
- **Until the handshake exists**, document the current implicit contract
  (bincode variant-append discipline) in `PROTOCOL.md` as the *legacy*
  compatibility rule, and treat it as the thing being replaced — not relied on by
  any external client.

---

## Constraints

- **Light deps:** the Python client is **stdlib-only** (`socket`, `json`,
  `struct`, `pathlib`) — no compiled extension, no `fff` Rust artifacts in the
  consumer's dependency tree. (The rejected Rust-client-crate + C-ABI-shim route
  is the only shape that would break this — hence its rejection.)
- **Wide compatibility:** `requires-python >= 3.9`; pure-Python; runs on Linux/CI
  and under the system Python, not just the developer's machine. (Mirrors
  turbo-rag-client's `>= 3.9` target.)
- **Dogfood:** the engine and `fff-mcp`'s `EngineClient` speak the **same**
  published protocol/reference implementation in `fff-ipc` — one wire format, no
  second copy. After the work, `fff-mcp` is *a consumer of the documented
  contract* like everyone else.
- **Backward-compat / migration safety:** introduce the JSON envelope +
  `protocol_version` as a **dual-read** transition — the engine accepts both the
  legacy bincode frames and the new versioned frames until all in-flight
  clients (editor, MCP bridge, `fffctl`) have aged onto the new path. Do not
  remove the legacy path until then. (This directly answers "what happens to a
  daemon already running an older protocol?")
- **Respect the two surfaces:** the client targets the **daemon socket** (shared
  warm index + shared frecency), *not* the `fff-c` in-process surface. Do not
  conflate them; the FFI cdylib stays the in-process binding for JS/Bun/embedders.

---

## Tests

- **Import under 3.9, nothing heavy installed:** `python3.9 -c "import
  fff_client"` in a bare env — must succeed (no compiled deps).
- **Handshake + round-trip against a fake engine:** a fake asyncio/`socketserver`
  Unix-socket server that speaks the documented protocol — assert the two-phase
  handshake completes, `find_files`/`grep` resolve to typed results, and
  `record_access` writes without awaiting a reply.
- **Version-skew fails loud:** the fake server advertises an incompatible
  `protocol_version`; assert the client raises `FffError(PROTOCOL_MISMATCH)` and
  does **not** return partial/garbled results.
- **Live round-trip (integration):** against a real `fff-engine` started for a
  temp repo — `find_files`/`grep` return expected paths; `frecency_score` is
  populated; results match what the MCP tools return for the same query.
- **Engine-side dual-read:** the engine answers both a legacy frame and a new
  versioned frame during the transition window (migration-safety regression).
- **`fff-mcp` regression:** after `EngineClient` switches to the shared
  reference implementation, the existing `fff-mcp` tests still pass.

---

## Out of scope

- Changing fff's **search semantics** (ranking, frecency, query parsing, glob).
- Changing the **MCP tool surface** itself (`crates/fff-mcp/src/server.rs`
  tool definitions and their argument shapes).
- The **`fff-c` FFI / JS / Bun** in-process bindings — a separate surface with
  its own (`FFF_CREATE_OPTIONS_VERSION`) versioning; untouched here.
- The master/worker sharding model (ADR-001) — the client consumes it, does not
  alter it.
- Vendoring a client inside memory-kit — this spec **replaces** the need for it.

---

## Cross-references

- **memory-kit ADR-022** — "Retrieval Backend Behind a Port" (the consumer side;
  its `FffAdapter` will import `fff_client`, alongside `TurboRagAdapter`, fused
  by the `CompositeBackend` via Reciprocal Rank Fusion).
- **turbo-rag client spec** —
  `~/my-workspace/ai/turbovec/SPEC-turbo-rag-client-extraction.md` (the sibling;
  the schema-version handshake + refuse-on-mismatch is the model cited here).
- **turbo-rag ADR-008** — "Split Daemons (Engine + MCP Client)" — why a socket
  protocol exists at all (the analogue of fff's ADR-001).
- **fff ADR-001** — "fff-engine Worker Model — Master + Sharded OS-Process
  Workers" (`docs/adr/ADR-001-engine-worker-model.md`) — the daemon architecture
  this client consumes.
- **Recommendation:** record this on the fff side as a new ADR, e.g.
  **"ADR-002: Publish a documented, versioned protocol + deps-light client
  (`fff-client`)"** — covering the JSON-envelope + `protocol_version` handshake
  decision and the dual-read migration.

---

## Open questions for the owner

*(Scope is settled: a JSON-envelope wire contract + a stdlib Python client,
in-repo under `clients/python/`. The remaining questions are protocol/behavior
details, not shape.)*

1. **`list_roots` over the protocol.** It is currently a `fff-mcp` registry
   concept (startup config + `--root`), not a first-class IPC request. Should the
   protocol expose a `list_roots`/route-enumeration verb so a non-MCP consumer can
   discover targets, or does the consumer supply `base_path` out-of-band?
2. **Spawn responsibility.** `EngineClient::connect` spawns `fff-engine --master`
   if absent. Should the published Python client also spawn the daemon (needs the
   `fff-engine` binary on `PATH`), or only connect-and-fail if the daemon is not
   already running (cleaner boundary, but the consumer must ensure the daemon)?
3. **Frecency writes from a retrieval consumer.** Should memory-kit's reads call
   `record_access` (feeding fff's frecency so future rankings reflect what the
   agent retrieved), or stay read-only to avoid polluting the human's frecency
   signal?
