---
section: "Architecture"
type: adr
status: accepted
audience: internal
tags: [fff-ipc, protocol, versioning, python-client, dual-read, ipc]
---

# ADR-002: Versioned JSON Wire Protocol + Deps-Light Python Client

**Date:** 2026-06-13
**Status:** Accepted
**Deciders:** Abhijit Salvi

## Context

The `fff-engine` daemon is the warm, shared, frecency-ranked search surface the
fff MCP (Model Context Protocol) tools call. It speaks an IPC (Inter-Process
Communication) protocol over Unix sockets: a master socket for the two-phase
handshake and per-worker sockets for search traffic (see ADR-001).

That protocol had a blocking defect for any consumer outside the Rust workspace:
the handshake **carried no version field**, and the wire was **bincode encoded by
enum variant ordinal**. bincode is not self-describing — the decoder infers
meaning purely from byte position and declared types. The only compatibility
mechanism was a social rule in the type comments: *append new variants last so
ordinals never shift.* A client and engine that disagreed on the variant layout
did **not** fail loud — they silently misinterpreted bytes. Any externally shipped
client would be one `brew upgrade` or one stale daemon away from silent
corruption.

memory-kit (a separate Python project) is the forcing function. It is building a
`CompositeBackend` that fuses fff (lexical / frecency) with turbo-rag (semantic)
behind one retrieval port (memory-kit ADR-022). For the fff leg it needs the
**same rankings the MCP tools return**, which only the socket daemon provides —
the `fff-c` FFI (Foreign Function Interface) cdylib is a distinct in-process
surface with its own cold index and private frecency, explicitly not the target.
A Python consumer had no first-class path to the daemon: the only routes were to
hand-roll the unversioned bincode handshake, load the FFI cdylib (losing the warm
shared index), or shell out to a search CLI that does not exist. turbo-rag already
solved the equivalent problem with a `schema_version` + `SCHEMA_MISMATCH` refusal;
fff needed the same.

## Decision

Introduce a **versioned JSON envelope** accepted on the existing engine sockets
**alongside** bincode, and ship a stdlib-only Python client that speaks it.

- **Dual-read by first-byte content sniff.** The frame keeps its existing
  `[u32-LE len][payload]` shape — no discriminator byte is added. The receiver
  reads one frame and inspects the first non-whitespace byte: `{` (`0x7B`) routes
  to the JSON envelope; any other byte routes to the unchanged bincode path.
  Legacy first-message variant ordinals are all `≤ 0x0A`, so the sniff is
  collision-free.
- **`PROTOCOL_VERSION` single-sourced in `fff-ipc`.** A `u32` in `protocol.rs`,
  mirrored (never redefined) by the migrated `fff-mcp` `EngineClient` and the
  Python client. The envelope carries it on every request and response.
- **Refuse-on-mismatch, loud.** An incompatible `protocol_version` yields a typed
  `PROTOCOL_MISMATCH` error (carrying both versions), then the connection closes.
  No best-effort decode, ever — validated at both the master handshake and the
  worker connect.
- **A stdlib-only Python `fff-client`** under `clients/python/` —
  `socket`/`json`/`struct`/`pathlib` only, `requires-python >= 3.9`, no compiled
  artifact, no Rust toolchain in the consumer's dependency tree.
- **Dogfood.** The `fff-mcp` `EngineClient` is migrated onto the same `fff-ipc`
  reference implementation for its hot path, so there is one wire format with no
  second copy.

Three owner decisions made this session are part of this record:

- **`list_roots` is a first-class wire verb** (a master-socket verb), sourced from
  the configured `[mcp]` roots unioned with the live routing-table base paths —
  not a client-side reconstruction.
- **The Python client is connect-and-fail**: it resolves and connects to an
  already-running daemon and raises `FffUnreachable` if absent. It never spawns
  `fff-engine` and never requires the binary on `PATH`.
- **`record_access` is shipped but opt-in, default off**: present on the client
  surface, gated behind an explicit flag, so agent-driven retrieval does not
  pollute the human frecency signal unless deliberately enabled.

The full contract is documented in `crates/fff-ipc/PROTOCOL.md`.

## Consequences

### Positive

- **Real fff rankings for external consumers.** A Python process gets the same
  frecency-ranked, git-status-annotated results as the MCP tools, against the warm
  shared index.
- **Loud on skew.** Version disagreement surfaces as a typed `PROTOCOL_MISMATCH`
  instead of silent byte misinterpretation — the defect that blocked shipping any
  external client.
- **Migration safety.** Dual-read means a new engine keeps serving every existing
  legacy bincode client (editor, `fffctl`, un-migrated `fff-mcp`) byte-for-byte
  unchanged. No flag day.
- **Reorder-resilient wire.** The envelope is name-tagged (`verb` is a string),
  not ordinal-fragile, so future verbs can be added without the append-last dance
  on the JSON side.
- **One source of truth.** Dogfooding the reference implementation in `fff-mcp`
  means field names and shapes cannot drift between the engine, the canonical
  client, and the published contract.

### Negative / trade-offs

- **Two encodings during the transition.** The engine carries both a JSON and a
  bincode decode path until the legacy clients age out and the bincode path is
  removed in a later increment.
- **bincode ordinal discipline still governs the legacy side.** Until the bincode
  path is removed, existing variants must not be reordered, and any new
  first-message variant must keep an ordinal `< 0x7B` to preserve the sniff
  invariant.
- **Socket-path-resolution parity must be maintained.** The Python
  `default_socket_paths()` must mirror `fff_ipc::paths` precedence exactly; if it
  drifts, the client silently fails to find a running daemon. A parity test pins
  it.

### Neutral

- The `fff-c` / JS / Bun in-process surface and its create-options versioning are
  untouched; this work is daemon-socket only.
- `fffctl` is not migrated this increment — it stays on legacy bincode, which the
  dual-read engine still serves. Migration is deferred.
- The MCP tool surface seen by Claude Code is unchanged — only the underlying call
  path moved.

## Alternatives Considered

### Alternative A: Hand-roll bincode in Python

Re-derive the wire format in Python and speak bincode directly. Rejected: it
re-creates an undocumented, ordinal-fragile encoding with the exact silent-skew
failure mode we are removing, and pins the Python code to bincode's internal
representation.

### Alternative B: Rust client crate + cdylib / pyo3 shim

Keep bincode internal and expose it to Python through a compiled shim. Rejected:
it forces a per-platform compiled artifact and a Rust toolchain onto the
consumer — precisely the coupling memory-kit needs to avoid — for no protocol
benefit.

### Alternative C: A `fff search` CLI emitting JSON

Add a subcommand that prints JSON results, and have Python shell out. Rejected as
the primary path: process-spawn-per-query is heavyweight, it is awkward for
fire-and-forget `record_access`, and it does not give a typed mismatch. Fine as a
later scripting affordance; not chosen here.

### The transport alternative: a second versioned socket

Rather than content-sniffing one socket, the engine could expose a second
`master-v2.sock` for the versioned protocol. Rejected in favor of the first-byte
sniff: a second socket doubles socket lifecycle, path resolution, and spawn logic,
whereas the sniff is a zero-framing-change addition that leaves the bincode path
byte-identical and is provably collision-free.

## References

- ADR-001 — fff-engine Worker Model (master + sharded workers); the socket
  topology this protocol rides over.
- `crates/fff-ipc/PROTOCOL.md` — the language-neutral wire contract.
- `docs/plans/2026-06-13-001-feat-fff-versioned-protocol-python-client-plan.md` —
  the implementation plan (units U1–U8, the KTDs).
- `SPEC-fff-client.md` — the originating spec.
- memory-kit ADR-022 — the consumer-side `CompositeBackend` / `RetrievalBackend`
  port this client feeds.
