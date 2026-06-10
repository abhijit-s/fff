# Cold-start grep readiness race — investigation & fix

Date: 2026-06-10
Branch: worktree-agent-acd77f0d090079a57 (off main @ ddc7cd3)

## Summary

REPRODUCED: **Yes**, via a controlled readiness seam. Immediately after a cold
engine start, content `grep` can return empty or partial results that look
authoritative, then heal once indexing finishes. Root cause is that the initial
scan runs on a background thread *after* the engine begins answering queries.

Fix: a bounded engine-layer readiness gate (`await_index_ready`) blocks `grep`
and `multi_grep` until the picker reports `is_index_ready()`. No wire-API change.

## The exact warmup gap

`FilePicker::new_with_shared_state` (file_picker.rs:721) publishes an **empty**
picker into `SharedFilePicker` (line 750), spawns the initial `ScanJob`
(line 774), and returns immediately. `state::init` returns, the worker sends
`Ack`, and the connection starts accepting `Grep` requests — all while the scan
thread is still running.

The scan thread (`ScanJob::run`, scan.rs) has two distinct windows where a grep
issued against the live picker is wrong:

1. **Empty-file-list window** — before `commit_new_sync` (scan.rs:186),
   `get_files()` returns `&[]`. A content grep prefilters zero files and returns
   **0 matches**. This is the silently-empty result confirmed live (`McpConfig`
   → 0 right after reconnect). This is the real empty-result bug.

2. **Post-scan window** — after `commit_new_sync`, `scanning` is flipped to
   `false` *early* (scan.rs:199), then `run_post_scan` builds the bigram index
   and runs binary classification. During this window `bigram_index` is `None`,
   so `prefilter_files` takes the `None` arm and scans ALL live files. Confirmed:
   that arm returns CORRECT (slower) results — so window 2 alone does NOT cause
   empty results, consistent with the prior agent's claim. The gate still treats
   it as not-ready so the first query never observes a half-built index.

Ranking: window 1 (empty file list) is the cause of empty/partial results.
Window 2 is correct-but-slow. The `set_binary` classification gap does not cause
empty results either — unclassified files are treated as non-binary and remain
searchable.

## Readiness predicate

Added `FilePicker::is_index_ready()` (file_picker.rs), composed from existing
signals:

```
!is_scan_active() && !is_post_scan_active()
    && (!enable_content_indexing || bigram_index.is_some())
```

- `!is_scan_active()` — walk + `commit_new_sync` done (file list populated).
- `!is_post_scan_active()` — bigram build / classification post-scan done.
- bigram present when content indexing is on — the index that makes content
  grep complete actually exists.

No premature pass: `ScanJob::spawn` sets `scanning=true` synchronously before
spawning the thread, and `new_with_shared_state` calls `.spawn()` before
returning, so by the time `init()` returns `scanning == true` deterministically.

## Fix (chosen approach: (a) block until readiness)

`crates/fff-engine/src/handlers.rs`: `await_index_ready(state)` polls
`is_index_ready()` and is awaited at the top of `handle_grep` and
`handle_multi_grep` before the blocking search runs.

- Bounded by `GREP_READINESS_TIMEOUT` (30s); a pathologically slow scan yields
  correct-but-possibly-partial results rather than hanging.
- Poll interval 20ms.
- Lock discipline: the read lock is taken briefly each tick to read the atomic
  flags, then dropped *before* `tokio::time::sleep`. The lock is never held
  across an await. No new mutex/condvar introduced.

Why no API change: the gate lives entirely in the engine handler layer and uses
a new `pub` method on `FilePicker`. `SearchRequest`/`SearchResponse`/
`FindOptions`/`GrepOptions` are untouched. Per coordination note, the readiness
predicate composes existing `FilePicker` signals; the only `FilePicker` addition
is the read-only `is_index_ready()` accessor (no new field, no `grep.rs` matching
change), minimising collision with the concurrent `watch_coverage_degraded` work.

## Reproduction (deterministic seam)

`crates/fff-core/src/grep.rs` tests:

- `test_index_ready_false_before_filelist_committed` — content-indexing picker
  with no committed file list: `is_index_ready()` is false AND grep returns
  **0 matches** for a term present on disk. Reproduces the live bug.
- `test_index_ready_flips_when_bigram_built` — after `collect_files` the file
  list exists but no bigram index → not ready; after `set_bigram_index` →
  ready, and grep returns both matches.
- `test_index_ready_true_without_content_indexing` — readiness hinges only on
  scan completion when content indexing is off.

## Verification

- `cargo build --no-default-features -p fff-search -p fff-engine -p fff-ipc` — ok
- `cargo test --no-default-features -p fff-search` — 95 lib tests pass (incl. 3
  new), all integration suites pass.
- `cargo test --no-default-features -p fff-engine` — 21 pass.
- `cargo clippy --no-default-features -p fff-engine -p fff-search --tests` — no
  new warnings in handlers.rs / file_picker.rs / new grep tests (only
  pre-existing warnings in unrelated test files).
- `cargo fmt` — applied.

## Lock / perf notes

- First-query latency: the first grep after a cold start now waits up to the
  initial scan + bigram build duration (typically sub-second to a few seconds on
  large repos), polled at 20ms granularity. Warm queries are unaffected — the
  flag check is a few relaxed atomic loads and returns immediately.
- Hot path: `is_index_ready()` is three atomic loads + one `Option::is_some`;
  negligible. The gate adds one brief read-lock acquisition before the search.
- No lock held across `.await`.

## Residual gap

- The gate covers the *initial* cold-start scan. A later full rescan (e.g. a
  large directory move) flips `scanning`/`post_scan` again; a grep landing
  mid-rescan would also wait, which is the desired behaviour. Incremental
  watcher updates don't toggle these signals, so steady-state edits are not
  gated (correct — the overlay keeps results accurate).
- The 30s bound is a safety valve; if a genuinely huge repo exceeds it the first
  query may still return partial results, but only after a long wait rather than
  silently on a cold engine.
