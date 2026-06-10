# Content-Truncation Hardening — Implementation (Candidate 1)

Date: 2026-06-10
Branch: `worktree-agent-abf8617c7f8d2c27f` (from `main` @ ddc7cd3)
Crate: `fff-search` (directory `crates/fff-core`)
Implements: `docs/plans/2026-06-10-007-truncation-hardening-ideate-and-plan.md`
Builds on the investigation: `docs/plans/2026-06-10-005-content-staleness-investigation.md`

## What shipped

Candidate 1 — degraded-root-gated `(mtime, size)` recheck on the grep read path.

On a HOT root a dropped filesystem-watcher event leaves `FileItem.size` and the
persistent mmap cache stale, so `FileItem::get_content_for_search` bounded the
read by the stale size and silently truncated appended/changed content. The fix
surfaces the watcher's existing `aborted_early` signal as a per-root
`watch_coverage_degraded` flag and, ONLY when set, re-stats each
prefilter-surviving candidate and re-reads fresh bytes when on-disk
`(mtime, size)` disagree with the cached `FileItem`. Healthy roots keep the
byte-for-byte zero-syscall fast path (one relaxed atomic load per grep).

## Files changed

- `crates/fff-core/src/file_picker.rs`
  - `FilePicker` gains `watch_coverage_degraded: Arc<AtomicBool>` (private),
    initialized `false` in `new`.
  - `pub(crate) fn watch_coverage_degraded()` (relaxed load),
    `pub(crate) fn watch_coverage_handle()` (returns the `Arc` for the watcher),
    and a `#[cfg(test)] pub(crate) fn set_watch_coverage_degraded()` test seam.
  - `grep` and `multi_grep` pass `self.watch_coverage_degraded()` into the
    search functions.
- `crates/fff-core/src/types.rs`
  - `get_content_for_search` takes a new `recheck: bool`. When `true` it stats
    the file once; if `(mtime_secs, len)` differ from the cached `FileItem` it
    bypasses the persistent cache and reads FRESH bytes bounded by the on-disk
    size into the caller-owned `buf` / `mmap_slot` — NEVER the shared
    `OnceLock`. When `false` the path is byte-for-byte identical to before.
- `crates/fff-core/src/grep.rs`
  - `GrepContext` gains a `recheck: bool`; both `GrepContext` literals set it.
  - `grep_search`, `multi_grep_search`, and `fuzzy_grep_search` take `recheck`
    and thread it to both `get_content_for_search` call sites. The flag is read
    once per grep (in `FilePicker::grep`/`multi_grep`), not per file.
- `crates/fff-core/src/background_watcher.rs`
  - The NonRecursive watch loop is extracted into `watch_all_dirs(...) -> bool`
    (returns `aborted_early`), used by both the initial setup and the recovery
    re-watch so detection and recovery use identical logic.
  - Initial setup stores the loop result onto the picker flag.
  - Linux owner thread: on each new-dir event, if the picker is currently
    degraded, it re-runs `watch_all_dirs` and stores the fresh result —
    self-clearing (see below).
- `crates/fff-core/src/lib.rs` + `crates/fff-core/src/content_staleness_recheck.rs`
  - New `#[cfg(test)]` in-crate test module (kept in-crate so the test seam
    stays `pub(crate)` and the public/FFI surface is unchanged).

## Self-clearing semantics (human override — NOT a one-way latch)

The flag is a self-clearing `AtomicBool`, not a latch:

- **Set** when a watch (re)setup's `watch_all_dirs` loop reports
  `aborted_early == true` (a run of failures hit the per-process watch cap, e.g.
  inotify ENOSPC). Stored via `store(aborted_early, Relaxed)`.
- **Cleared** when a subsequent FULL re-watch completes WITHOUT aborting. The
  exact recovery condition implemented: the Linux watcher owner thread, on each
  new-directory event, checks `watch_coverage_degraded()`; if set, it re-runs
  the entire `watch_all_dirs` loop over every indexed directory and stores its
  fresh `aborted_early`. A clean pass (`false`) means every directory was
  successfully (re)registered with the kernel — i.e. the watch table regained
  capacity — which is a credible proof that coverage recovered, so the flag
  clears. If the re-watch still aborts, the flag stays set.

This is implemented at the watcher layer; the read-path tests exercise the
read-side consequence (clear flag ⇒ fast path restored) directly via the test
seam.

### Residual gap (surfaced honestly)

The recovery re-watch is triggered by the owner thread, which only wakes on
new-directory `Create` events arriving through `watch_tx`. On a degraded root
the directories that failed to register emit no events, so recovery is
**opportunistic**: it fires the next time ANY still-watched directory produces a
new-dir event after kernel capacity frees up. Until such an event occurs, a
recovered root keeps paying the per-stat recheck cost (correctness is never at
risk — only the cost lingers). There is no standalone timer that proactively
retries; adding one is the deferred Candidate 3 background-sweep layer. A full
rescan/reload also resets the flag implicitly because it constructs a fresh
`FilePicker` (flag defaults `false`) and re-runs initial watch setup.

Why this is acceptable over a latch: while degraded, the recheck GUARANTEES
correct results regardless of recovery timing. Clearing only removes the
per-stat cost once coverage is provably back. A latch would never reclaim the
fast path without a full rescan; self-clearing reclaims it as soon as a clean
re-watch is observed.

## Perf / lock notes

- Healthy path: one relaxed atomic load per grep call; per-file behavior
  byte-identical to before (zero syscalls on a cache hit).
- Degraded path: at most one `stat` + at most one transient `open`/`mmap` per
  candidate that survives the bigram prefilter. No per-line syscalls.
- Locking: the recheck runs under the existing shared picker READ lock; it adds
  a `stat` of the same nature as the `open`/`mmap` the function already performs
  on its slow branches. No write lock on the grep hot path; `FileItem` is never
  mutated on the read path (stays `&self`). The watcher sets/clears the flag via
  the `Arc<AtomicBool>` with no I/O held under any lock. The owner-thread
  recovery holds the debouncer mutex then a picker READ lock; this is
  deadlock-free because the debouncer event handler never acquires the debouncer
  mutex, so there is no lock cycle.

## Tests (all passing, in-crate `cargo test --no-default-features -p fff-search --lib`)

- `dropped_modify_event_refreshes_on_degraded_root` — the promoted repro: a
  dropped append on a degraded root is now found.
- `same_size_inplace_edit_refreshes_on_degraded_root` — same-byte-length
  in-place edit is caught via the `(mtime, size)` stat (half-fix detector).
- `healthy_root_stays_fast_path_degraded_root_refreshes` — flag clear ⇒ stale
  (no re-stat); flag set ⇒ refreshes. Proves the gate gates.
- `self_clearing_flag_returns_to_fast_path` — degraded refresh works; after the
  flag clears, a further dropped edit is served from the (stale) fast path,
  proving the cleared flag restores the zero-syscall path.

## Breaking-change rule (honored)

All new state is private or `pub(crate)`; `get_content_for_search` is
`pub(crate)`. No FFI/C/Lua/Bun signature changed. Grep result shape is
unchanged — only correctness on a degraded root improves.

## Coordination note

`grep.rs` changes were kept minimal (threading one `bool` field into
`GrepContext` and through the three search entry points) to minimize conflict
with the concurrent `grep.rs` query-false-negative fix. No matching logic was
refactored.

## Glossary

- **FFI** — Foreign Function Interface.
- **MCP** — Model Context Protocol; the connection that keeps a root "hot".
- **mmap** — memory-mapped file (`memmap2::Mmap`); the persistent content cache.
- **ENOSPC** — POSIX "no space left" errno; here the inotify watch-limit error.
- **mtime** — file modification time (compared at seconds granularity, matching
  the index's scan-time stamp).
