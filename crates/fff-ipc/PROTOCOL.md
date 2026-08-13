# fff Engine Wire Protocol

The stable, language-neutral contract for talking to the **fff engine daemon** —
the warm, shared, frecency-ranked surface the fff MCP (Model Context Protocol)
tools use. This is the protocol spoken by the engine's JSON dual-read path, the
migrated `fff-mcp` `EngineClient`, and the stdlib-only Python `fff-client` under
`clients/python/`.

The reference implementation lives in this crate: `src/protocol.rs` (envelope
types, version, verbs, error codes, sniff/version helpers) and `src/codec.rs`
(framing and the dual-read primitives). `PROTOCOL_VERSION` is the single source
of truth; every consumer pins to it and refuses to talk to a mismatched peer.

---

## Framing

Every message — bincode or JSON, request or response — is a single length-prefixed
frame:

```
[ 4-byte little-endian u32 payload length ][ payload bytes ]
```

The length prefix counts only the payload, not itself. The JSON read path rejects
a declared length greater than `MAX_FRAME_LEN` (64 MiB) with a typed
`FrameTooLarge` error **before allocating**, guarding against a hostile or garbled
length prefix. First-message requests are tiny, so the guard never trips
legitimate traffic.

There is no type tag in the frame. The decoder is selected by content sniff (next
section).

---

## Dual-read sniff invariant

A single engine socket accepts **both** the versioned JSON envelope (current) and
the legacy bincode encoding (superseded, still served during the transition). The
receiver reads one frame's payload bytes and inspects the **first non-whitespace
byte**:

| First byte           | Decoder                                  |
| -------------------- | ---------------------------------------- |
| `{` (`0x7B`)         | JSON envelope (versioned protocol)       |
| anything else        | legacy bincode (`MasterRequest` / `SearchRequest`) |

This is `looks_like_json` in `protocol.rs`. The session stays in whichever mode it
opened in — a JSON connection answers JSON for its lifetime; response encoding
always mirrors request encoding.

### Why the sniff is collision-free

bincode encodes a leading enum variant as a little-endian `u32`, so the first
payload byte of any legacy first-frame equals that variant's ordinal. The
reachable first-message variants are:

- `MasterRequest` variants `0`–`6` (`Handshake` … `Health`)
- `SearchRequest::{Connect, Health, DropRoot}` (ordinals `8`, `9`, `10`)

Every reachable first byte is `≤ 0x0A`. `0x7B` (`{`) can never be a legacy first
byte, so the JSON-vs-bincode test cannot collide.

> **Hard invariant.** This collision-freedom is load-bearing, not incidental. Any
> new bincode `MasterRequest` / `SearchRequest` variant that could appear as the
> **first** frame on a connection MUST keep its ordinal below `0x7B`. In practice
> this is automatic — there is enormous headroom below 123 variants — but a
> protocol change that reorders variants or pushes a first-message variant to
> ordinal `0x7B` would silently break the sniff. Do not reorder existing variants
> (see *Legacy bincode discipline* below).

---

## Two-phase handshake

A client reaches a worker through the master, then talks to the worker directly.
The master is a thin router and is not in the per-request hot path.

```
client                         master.sock                 worker-N.sock
  │  {pv, verb:"handshake",        │                            │
  │   params:{base_path}} ───────▶ │  version check             │
  │ ◀── {worker_socket, idx} ──────│                            │
  │                                                             │
  │  {pv, verb:"connect", params:{base_path}} ────────────────▶ │ version check
  │ ◀──────────────────────────────────── {ok, result:{ack}} ──│
  │  {pv, verb:"grep", params:{…}} ───────────────────────────▶ │
  │ ◀──────────────────── {ok, result: WireGrepResponse} ───────│
```

1. **Connect the master socket** (`<cache>/fff/master.sock`) and send a
   `handshake` request carrying `{base_path}`. The reply is a `HandshakeResult`
   `{worker_socket, worker_index}`. Close the master connection.
2. **Connect the returned worker socket** and send a `connect` request carrying
   `{base_path}`. The reply is `{ok: true, result: {"ack": true}}`. The worker
   loads (or reuses) the root's index on demand.
3. **Send verb frames** on the worker connection for the rest of the session.

`protocol_version` is validated at **both** the master handshake **and** the
worker connect, so each socket defends itself independently. On a version
mismatch the engine writes a `PROTOCOL_MISMATCH` error envelope (carrying both
versions) and **closes the connection** — refuse-on-mismatch, loud. There is no
best-effort decode and no partial result.

`master_socket_path()` resolves `<cache>` via the precedence `$XDG_CACHE_HOME` →
`$HOME/.cache` → platform cache dir → `/tmp`, then `fff/master.sock`. A consumer
never derives a worker socket path itself; it uses the path the handshake
returns.

---

## Envelope schemas

All three envelopes carry `protocol_version` (a `u32`).

**Request**

```json
{ "protocol_version": 1, "verb": "<name>", "params": { … } }
```

`params` defaults to `{}` when omitted.

**Response — ok**

```json
{ "protocol_version": 1, "ok": true, "result": <verb-specific JSON> }
```

**Response — error**

```json
{
  "protocol_version": 1,
  "ok": false,
  "error": {
    "code": "<CODE>",
    "message": "<text>",
    "engine_version": 1,
    "client_version": 1
  }
}
```

`engine_version` / `client_version` are present only on `PROTOCOL_MISMATCH`;
otherwise omitted. Exactly one of `result` (when `ok`) or `error` (when `!ok`) is
present.

`result` payloads are the serde-JSON of the existing `Wire*` result structs — no
parallel result types exist. The same structs serialize as bincode on the legacy
path and as JSON here.

---

## Verbs

Verbs are split across the two sockets. A verb sent to the wrong socket is a
`BAD_REQUEST`.

### Master-socket verbs

Sent on `master.sock`, one request per connection.

| Verb         | `params`        | `result`                                              |
| ------------ | --------------- | ----------------------------------------------------- |
| `handshake`  | `{base_path}`   | `HandshakeResult` `{worker_socket, worker_index}`     |
| `health`     | `{}`            | `HealthReport` (see below)                            |
| `list_roots` | `{}`            | `Vec<WireRoot>` (see below)                           |

`list_roots` returns `[{base_path, name?, default}]`, **default-first**, paths
canonicalized. It is the union of the configured `[mcp]` roots (carrying `name`
and `default`) and any live-but-unconfigured routing-table base paths
(`name: null`, `default: false`), de-duplicated by canonical path (a configured
root that is also live appears once and keeps its `name`/`default`).

`HealthReport` is `{master_pid, uptime_sec, workers: [{index, pid, roots:
[RootHealth]}]}`, where `RootHealth` is `{slug, base_path, indexed_files?,
last_scan_age_sec?, watcher_backlog?, dirty_count?, last_access_age_sec?}` (numeric
fields are nullable; `null` means "not measured", not zero). `last_access_age_sec`
is seconds since the root last served a query and drives idle-root eviction.

### Worker-socket verbs

The **first** worker frame must be `connect` or `health`; any other first verb is
rejected and the connection closed. After a successful `connect`, the remaining
verbs may be sent repeatedly on the same connection.

| Verb                | `params`                                          | `result`                                |
| ------------------- | ------------------------------------------------- | --------------------------------------- |
| `connect`           | `{base_path}`                                     | `{"ack": true}`                         |
| `health`            | `{}` (first message only; one-shot, then close)   | `HealthResponse` `{roots: [RootHealth]}`|
| `grep`              | `{query, options?}` (`GrepOptions`)               | `WireGrepResponse`                      |
| `find_files`        | `{query, options?}` (`FindOptions`)               | `Vec<WireSearchResult>`                 |
| `multi_grep`        | `{patterns, constraints?, options?}` (`GrepOptions`) | `WireGrepResponse`                   |
| `list_recent_files` | `{limit, dirty_only?}`                            | `Vec<WireSearchResult>`                 |
| `get_git_status`    | `{include_clean?}`                                | `Vec<WireGitFile>`                      |
| `list_directories`  | `{limit}`                                         | `Vec<WireDirEntry>`                     |
| `record_access`     | `{path}`                                          | **no response frame** (fire-and-forget) |

`record_access` is fire-and-forget: the engine performs the frecency write and
sends **no** reply. A client must not block waiting for one.

### Result struct fields

The `Wire*` structs are defined in `src/types.rs`:

- **`WireGrepResponse`** — `{matches: [WireGrepFileMatches], total_files_searched,
  total_files, files_with_matches, next_file_offset, regex_fallback_error?}`.
  - **`WireGrepFileMatches`** — `{path, size, git_status?, frecency_score,
    matches: [WireGrepMatch]}`.
  - **`WireGrepMatch`** — `{line_number, col, line_text, match_byte_offsets:
    [[start, end]], is_definition, context_before, context_after}`.
- **`WireSearchResult`** — `{path, score, git_status?, frecency_score}`.
- **`WireGitFile`** — `{path, status, frecency_score}` (`status` is a label such
  as `"modified"` or `"untracked"`).
- **`WireDirEntry`** — `{path, max_frecency}`.
- **`WireRoot`** — `{base_path, name?, default}`.

`git_status` is a nullable `u32` status code; `frecency_score` is the sum of the
access and modification frecency scores.

### The `options` object is all-or-nothing

`GrepOptions` and `FindOptions` have **no per-field serde defaults**. The
`options` key as a whole is `#[serde(default)]` — so omitting it entirely is fine
(the engine applies its full defaults) — but a *partial* `options` object is
**rejected** with `BAD_REQUEST` ("missing field …"). A client that wants to
override a single field must send a **complete** object. The defaults are:

```text
GrepOptions  { max_file_size: 10485760, max_matches_per_file: 10,
               smart_case: true, file_offset: 0, page_limit: 50,
               mode: "PlainText", time_budget_ms: 0, before_context: 0,
               after_context: 0, classify_definitions: false,
               trim_whitespace: true }

FindOptions  { max_threads: 0, current_file: null,
               combo_boost_score_multiplier: 3, min_combo_count: 2,
               offset: 0, limit: 20 }
```

`mode` is one of `"PlainText"`, `"Regex"`, `"Fuzzy"`.

---

## Error codes

| Code                | Meaning                                                                 |
| ------------------- | ----------------------------------------------------------------------- |
| `PROTOCOL_MISMATCH` | The peer's `protocol_version` is incompatible. Carries `engine_version` and `client_version`. Refuse-on-mismatch, then close. |
| `BAD_REQUEST`       | The envelope or its params could not be understood — malformed envelope, unknown verb, wrong-socket verb, or undeserializable / partial params. |
| `INTERNAL`          | The handler failed while processing an otherwise valid request, or a result failed to serialize. |

---

## `PROTOCOL_VERSION` and the bump policy

`PROTOCOL_VERSION` (currently **1**) is single-sourced in `fff_ipc`
(`protocol.rs`) and mirrored — never redefined — by each external consumer
(`fff_client._protocol.PROTOCOL_VERSION`).

**Bump the version whenever any wire shape changes incompatibly** — an envelope
field, a verb's `params` shape, a `Wire*` result shape, or an error contract.
Client and engine compare versions at both handshake points and **refuse on
mismatch** rather than risk a silent misread. Ship engine and client together: a
client one version ahead of (or behind) the engine gets a clean
`PROTOCOL_MISMATCH`, not a garbled decode.

Talking to an engine too old to speak the JSON envelope at all (pre-versioned
protocol) does **not** yield a clean `PROTOCOL_MISMATCH`: that engine
bincode-decodes the `{…}` bytes and errors or drops the connection. A clean
mismatch requires an engine new enough to sniff JSON.

---

## Legacy bincode discipline (SUPERSEDED)

Before the versioned envelope, the engine spoke **only** bincode, encoded by enum
variant ordinal, with no version field. The single compatibility mechanism was a
social rule recorded in the type comments:

> Append new `MasterRequest` / `SearchRequest` / `SearchResponse` variants **last**
> so the bincode ordinals of existing variants never shift.

This rule is **superseded** by the versioned JSON envelope, which is name-tagged
(`verb` is a string, not an ordinal) and therefore reorder-resilient and
explicitly versioned. New external consumers use the JSON protocol exclusively and
must not depend on bincode ordinals.

The bincode path is **still accepted** during the dual-read transition (migration
safety): existing legacy clients — the editor integration, `fffctl`, and any
un-migrated `fff-mcp` build — keep working unchanged until they age out. Until the
bincode path is removed in a later increment, the append-last ordinal discipline
**still governs the bincode side**, and the sniff invariant above
(first-message ordinals stay `< 0x7B`) must be preserved. Do not reorder existing
variants while dual-read is live.
