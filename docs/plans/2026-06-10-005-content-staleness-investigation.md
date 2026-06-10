# Content-Index Staleness on a Hot Root — Investigation

Date: 2026-06-10
Branch: `worktree-agent-af6a745abea02a773` (from `main` @ ddc7cd3)
Crate under test: `fff-search` (directory `crates/fff-core`)

## Outcome

REPRODUCED: yes. NO production fix was made — see "Why no fix" below.

A deterministic failing test reproduces the reported symptom. New, valuable
regression tests were added; the reproduction itself is committed as a
`#[ignore]`d known-failing test so it can be run on demand without breaking CI.

## The reported symptom

On a hot root (loaded and held by a live MCP connection, never evicted),
`grep` for a newly-added identifier (`McpConfig`) returned 0 matches, while
older identifiers in the same files (`RootRegistry`, `FffConfig`) matched fine,
and `find_files` (the filename index) was correct. So the content index kept
pre-edit content for some files and the edit never refreshed it.

## Reproduction

Tests live in `crates/fff-core/tests/content_staleness_dropped_event.rs`. They
drive `FilePicker` directly with `watch: false`, so filesystem events are
delivered (or withheld) by hand.

| Test | Scenario | Result |
| ---- | -------- | ------ |
| `handled_modify_refreshes_content` | edit + call `handle_create_or_modify` | PASSES — new ident found |
| `dropped_event_small_file_bigram_only` | dropped event, target file < `MMAP_THRESHOLD` | PASSES — new ident found |
| `dropped_modify_event_leaves_stale_content` (`#[ignore]`) | dropped event, target file ≥ `MMAP_THRESHOLD`, dense corpus | FAILS — new ident returns 0 matches |

"Dropped event" = the file is edited on disk but `handle_create_or_modify` is
never called, modelling a missed / coalesced / dropped filesystem watcher event
on a root that stays loaded.

Run the failing repro on demand:

```
cargo test --no-default-features -p fff-search \
  --test content_staleness_dropped_event -- --ignored
```

## Root cause

When a watcher event for a modified file is **dropped**, two pieces of
per-file state are never refreshed:

1. `FileItem.size` — the recorded byte length, set at scan time.
2. The persistent content cache — an `mmap` stored in `FileItem.content`
   (a `OnceLock`), populated for files ≥ `MMAP_THRESHOLD` that fit the cache
   budget.

On the grep read path, `FileItem::get_content_for_search`
(`crates/fff-core/src/types.rs:696`) takes the persistent-cache fast path
(`get_cached_content`, line 709) and the search is bounded by the stale
`self.size`. So even though the kernel page cache behind the mmap may hold the
new bytes, grep only reads up to the OLD size and the appended `McpConfig` is
truncated away. The result: stale content, 0 matches for the new identifier.

The healthy path works precisely because `handle_file_modify`
(`crates/fff-core/src/file_picker.rs:1417`) calls
`FileItem::update_metadata` (refreshes `self.size`, invalidates the mmap) and
`BigramOverlay::modify_file` (re-extracts bigrams from fresh disk content).
None of that runs when the event is dropped.

### What was ruled OUT

- **Bigram prefilter exclusion (hypothesis 1/3).** Ruled out as the trigger:
  `dropped_event_small_file_bigram_only` keeps the bigram filter dense (200-file
  corpus) but uses a small target file (no content cache). The new identifier
  IS found there, so the prefilter does not exclude the modified file. The
  base bigram index is genuinely never updated on modify (only the overlay is),
  but the overlay merge in `grep.rs` is OR-based, so a stale base entry causes a
  false positive (extra file searched), never a false negative.
- **Atomic-save rename unhandled (prior agent's lead).** The watcher already
  stat-disambiguates macOS `Modify(Name(Any))`
  (`background_watcher.rs` ~462–495) and routes to `handle_create_or_modify`.
  Not the trigger.
- **Reload-after-eviction.** Settled by the prior agent; not re-investigated.

### Why missed events are realistic on a hot root

- Linux inotify ENOSPC: the NonRecursive watch loop aborts early after
  `MAX_CONSECUTIVE_WATCH_FAILURES` (`background_watcher.rs:280`,
  `aborted_early=true`), leaving directories permanently unwatched while the
  root stays "loaded". Edits there are silently missed.
- A new directory created at runtime under an aborted/unwatched parent never
  gets its own watch, so subsequent edits in it are missed.
- FSEvent buffer overflow is handled via the `Rescan` flag, but only while the
  stream is healthy.

There is no self-healing: nothing re-validates `FileItem` content against disk
after the watcher has missed an event.

## Why no fix was made

A correct fix must self-heal stale content without regressing the grep hot path
or holding a lock across file I/O. The candidates all fail at least one bar:

- Per-grep `stat` of every searched file turns the current zero-syscall
  persistent-cache hit into a one-syscall check per large file per query — a
  measurable latency regression on large-repo grep, explicitly out of bounds.
- A truncation-only check (mmap length vs `self.size`) is free but only catches
  shrinks; the common grow-in-place / same-size edit still serves stale bytes.
  A half-fix that silently misses the common case is worse than none.
- A periodic background revalidation sweep is a real feature with its own
  perf/lock design — speculative, beyond a minimal fix.

Per the investigation's decision rule (honesty over a forced fix; do not write
speculative production code), no production code was changed.

## Single most-likely remaining cause

Stale `FileItem.size` + persistent mmap cache after a **missed watcher event**
(predominantly Linux inotify watch-cap/ENOSPC abort, secondarily a runtime-new
directory under an unwatched parent). The content layer trusts the watcher
completely and never reconciles against disk.

## Recommended next probe

Add observability before any heal, so the failure can be confirmed in the wild:

1. A dropped/aborted-watch metric. The `aborted_early` branch in
   `background_watcher.rs` already knows when coverage is incomplete — surface
   it (counter + a "degraded watch coverage" flag on the root) instead of only
   logging. Expose via the engine's status so an MCP client can see "this root
   may serve stale content".
2. A cheap generation/mtime stamp on the content cache. Record the `mtime`
   alongside the cached mmap; on the read path compare only when a per-root
   "degraded coverage" flag is set (not on every grep), invalidating on
   mismatch. This bounds the stat cost to roots already known to have missed
   events, keeping the healthy hot path at zero syscalls.
3. If (1) confirms missed events are common, a low-frequency background
   revalidation sweep driven by the watcher thread (off the picker lock, via the
   existing `post_scan_snapshot` pattern) is the principled heal.

## Constraints honored

- No top-level Rust/Lua/C/Bun API changes.
- No production code changed; no new locks introduced.
- Tests only; default suite stays green (repro is `#[ignore]`d).
- All builds with `--no-default-features` (default needs Zig).
```

## Files

- `crates/fff-core/tests/content_staleness_dropped_event.rs` — reproduction + diagnostic tests.
- `crates/fff-core/src/types.rs:696` — `get_content_for_search` (stale-cache read path).
- `crates/fff-core/src/types.rs:608` — `update_metadata` (the refresh the dropped path skips).
- `crates/fff-core/src/file_picker.rs:1417` — `handle_file_modify` (healthy path).
- `crates/fff-core/src/background_watcher.rs:280` — watch-cap abort (missed-event source).
