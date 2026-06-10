# Content-Truncation Hardening — Ideate + Plan

Date: 2026-06-10
Branch: `worktree-agent-ae46afcda831cfe15` (from `main` @ ddc7cd3)
Crate: `fff-search` (directory `crates/fff-core`)
Builds on: `docs/plans/2026-06-10-005-content-staleness-investigation.md` (ground truth — root cause already settled there)

## Problem restatement (settled, not re-litigated)

On a HOT root (loaded, held by a live MCP — Model Context Protocol — connection,
never evicted), a missed / dropped filesystem-watcher event leaves the in-memory
index stale and grep silently TRUNCATES content:

- A dropped modify event means `handle_file_modify`
  (`crates/fff-core/src/file_picker.rs:1417`) never runs, so neither
  `FileItem.size` (`types.rs:216`) nor the persistent mmap content cache
  (`FileItem.content: OnceLock`, `types.rs:226`) is refreshed.
- The grep read path `FileItem::get_content_for_search` (`types.rs:696`) takes
  the persistent-cache fast path (`get_cached_content`, `types.rs:653`) and the
  whole search is bounded by the stale `self.size` (the `else` branch's
  `let len = self.size as usize;`, `types.rs:731`, and the prefilter/searcher
  operate only on the cached slice). Appended or changed content past the old
  size is invisible: a real symbol returns 0 matches, while the filename index
  and older symbols stay correct.
- Realistic trigger: Linux inotify ENOSPC aborts the NonRecursive watch loop
  (`background_watcher.rs:280`, `aborted_early=true`) leaving directories
  permanently unwatched while the root stays loaded; macOS FSEvents stream caps
  / coalescing can drop events similarly. There is no self-healing today.

This document is PHASE 1 (IDEATE) + PHASE 2 (PLAN). **No production code is
changed here** — the deliverable is the plan.

---

# PHASE 1 — IDEATE

Five candidate approaches, each assessed on: (a) correctness coverage — catches
BOTH truncation (append/shrink) AND same-size / grow-in-place edits;
(b) perf — syscalls added to the grep hot path, which is currently ZERO-syscall
on a persistent-cache hit; (c) complexity / blast radius; (d) locking — any risk
of holding a `Mutex` / `RwLock` across `stat` / file I/O (must be avoided);
(e) cross-platform (Linux inotify vs macOS FSEvents vs Windows).

A note used throughout: **mtime + size together** is the cheap correctness
signal. mtime alone misses same-second edits (mtime has 1s granularity on many
filesystems — the repro test itself sleeps 1100 ms to dodge this); size alone
misses same-size edits. The pair (mtime changed OR size changed) is what
`handle_file_modify` effectively trusts, and a single `stat` returns both. A
length-only check (mmap len vs `self.size`) is therefore explicitly a HALF-FIX:
it catches shrink/grow but is blind to a same-size in-place edit, which is the
common "fix a typo in an identifier" case. We reject any length-only design.

## Candidate 1 — Degraded-root-gated mtime/size recheck (RECOMMENDED)

Surface the already-computed `aborted_early` as a per-root "degraded watch
coverage" flag. On the grep read path, ONLY when that flag is set, `stat` each
candidate file and compare (mtime, size) against the cached `FileItem`; on
mismatch, drop the stale persistent mmap and re-read fresh, then search the
fresh bytes.

- (a) Coverage: FULL. A `stat` returns both mtime and size, so append, shrink,
  and same-size in-place edits are all caught. Not a half-fix.
- (b) Perf: the healthy common case (flag clear) keeps the exact current
  zero-syscall fast path — the gate is a single relaxed atomic load per grep,
  not per file. Cost is paid only on roots already known to have missed events,
  bounded to one `stat` per searched candidate after the bigram prefilter has
  already narrowed the set. This is the honest perf win over Candidate 2.
- (c) Complexity: moderate. One bool/atomic on the picker, one write from the
  watcher setup, one branch in the read path. Reuses the existing
  `update_metadata` + `invalidate_mmap` refresh primitive.
- (d) Locking: the grep path already holds the picker READ lock for the slice
  lifetime. The recheck does `stat` (a syscall) UNDER that read lock. This is
  acceptable ONLY because grep already does file I/O (`File::open` + `mmap`)
  under the same read lock on the budget-exhausted / fresh-mmap branches
  (`types.rs:723`, `types.rs:734`) — the read lock is shared, so concurrent
  greps don't serialize, and no WRITE lock is taken on the hot path. The refresh
  itself must NOT mutate `FileItem` under the read lock (that needs `&mut`); see
  the design note below — we refresh into a thread-local scratch mmap, not the
  shared `OnceLock`, so the read path stays `&self`.
- (e) Cross-platform: `std::fs::metadata` is portable. The flag is set on the
  Linux NonRecursive abort and can also be set on the macOS stream-failure path
  (`MAX_CONSECUTIVE_WATCH_FAILURES`). Windows never aborts the loop, so the flag
  stays clear there — correct, since Windows uses recursive watching with no
  per-dir cap.

## Candidate 2 — Always mtime/size-check every grep candidate

Same recheck, but unconditional (no gate).

- (a) Coverage: FULL (same `stat`-based comparison).
- (b) Perf: REGRESSION. Turns the current zero-syscall persistent-cache hit into
  one `stat` per large file per query. On a hot mono-repo grep over thousands of
  cached files this is a measurable, always-on latency tax to defend against a
  rare failure mode. The investigation explicitly ruled this out of bounds.
- (c)/(d)/(e): same shape as Candidate 1 but pays the cost on 100% of roots.

Rejected: pays a permanent tax on the healthy path to fix a degraded-path bug.

## Candidate 3 — Periodic background reconciliation sweep

A low-frequency timer on the watcher owner thread re-stats loaded files and
refreshes changed ones, independent of grep.

- (a) Coverage: FULL if it stats (mtime, size).
- (b) Perf: zero cost on the grep hot path (best on this axis). But it adds
  steady-state background syscall load proportional to index size, and a
  reconciliation window where grep still serves stale content until the next
  sweep — so it is eventually-consistent, not read-consistent.
- (c) Complexity: HIGH. A new scheduler, a sweep cadence to tune, interaction
  with the existing scan/rescan signals, and a budget so a huge index doesn't
  thrash. A genuinely new subsystem.
- (d) Locking: must refresh under a WRITE lock (mutating `FileItem.size` + mmap
  invalidation) — exactly the long-lock-while-doing-I/O hazard the project warns
  about. Mitigable via the existing `post_scan_snapshot` off-lock pattern, but
  that is more machinery.
- (e) Portable, but the sweep is pure overhead on healthy roots.

Rejected as the primary fix (speculative, high blast radius), but viable as a
LATER layer if telemetry shows missed events are common even on non-aborted
roots. Noted as a non-goal for this change.

## Candidate 4 — Watcher robustness fix (prevent dropped events)

Handle ENOSPC by escalating: raise-limit guidance, fall back to a recursive /
polling watch, or rescan the affected subtree so events aren't dropped at all.

- (a) Coverage: does NOT fix the read path. It reduces the FREQUENCY of the
  trigger but cannot eliminate it (FSEvents coalescing, polling-watch latency,
  a runtime-new dir under a parent that lost its watch). Any residual miss still
  yields silent truncation with no backstop.
- (b) Perf: a polling fallback is itself a steady `stat` load; a recursive Linux
  fallback wastes file descriptors on gitignored trees (the very reason the code
  chose NonRecursive — `background_watcher.rs:251`).
- (c) Complexity: HIGH and platform-forked (inotify limit handling vs FSEvents
  stream caps), and it changes the watch strategy that the codebase deliberately
  tuned.
- (d)/(e): largest blast radius of all options; touches the most load-bearing,
  platform-specific code.

Rejected as the primary fix: it is a probability reduction, not a correctness
guarantee, and grep would still silently truncate on any residual miss. The
right architecture is a correctness backstop on the READ path (Candidate 1)
that does not depend on the watcher being perfect. (Raising the inotify limit
remains a good OPERATIONAL recommendation to surface to users, and the degraded
flag from Candidate 1 is exactly the signal that lets us tell them — so the two
compose.)

## Candidate 5 — Fix the read path's size bound (grow-in-place detection)

Stop bounding `get_content_for_search` by `self.size` on the persistent-cache
path: re-read / re-map when the mmap length disagrees with `self.size`, or trust
the live mmap length instead of `self.size`.

- (a) Coverage: PARTIAL — a HALF-FIX. The kernel page cache behind the mmap may
  reflect appended bytes (so a length-based re-bound could catch some appends),
  but a same-size in-place edit changes NO length and is still served stale.
  Worse, an mmap's mapped length does not grow when the file grows after
  mapping — you must re-`stat` or re-`open` to learn the new length, at which
  point you are back to Candidate 1's `stat` anyway. Trusting raw mmap length
  also reintroduces the SIGBUS-on-truncate hazard the invalidation was designed
  to avoid (`types.rs:678`).
- (b) Perf: appears free but isn't — to learn the true current length you need a
  syscall, so it collapses into Candidate 1/2.
- (c)/(d)/(e): low complexity but the coverage gap (same-size edits) makes it a
  silent-half-fix, which the investigation explicitly calls worse than nothing.

Rejected: silently misses the common same-size edit.

## Recommendation

**Candidate 1 — degraded-root-gated mtime/size recheck.** It is the only option
that is (i) fully correct for all three edit shapes (append, shrink, same-size),
(ii) zero-cost on the healthy common path, and (iii) bounded in blast radius to
the read path plus one flag. Candidate 2 taxes the healthy path; Candidates 4
and 5 are half-fixes (frequency reduction / coverage gap) that still allow
silent truncation. Candidate 3 is the principled long-term heal but is a new
subsystem and eventually-consistent — deferred.

**Layering:** ship Candidate 1 now. Reuse its `aborted_early` flag as the signal
to ALSO emit the operational "raise `fs.inotify.max_user_watches`" guidance
(the useful, low-risk slice of Candidate 4). Leave Candidate 3 (background
sweep) as a documented future layer gated on telemetry from the new flag.

---

# PHASE 2 — PLAN (Candidate 1)

## Data flow

```
watcher setup (background_watcher.rs)
  aborted_early computed in the NonRecursive loop  ──► write degraded flag
                                                       onto the picker
                                                       (under the read/write
                                                        guard already held)
        │
        ▼
FilePicker  ── stores AtomicBool `watch_coverage_degraded`
        │      exposed via &self getter for the read path
        ▼
grep.rs   ── reads the flag ONCE per grep into GrepCtx (single atomic load)
        │
        ▼
FileItem::get_content_for_search(..., recheck: bool)
        │
   recheck == false (healthy): existing zero-syscall fast path, unchanged
   recheck == true  (degraded): stat the file; if (mtime,size) differ from
                                cached FileItem, read fresh into the per-thread
                                MmapSlot/buf and search those bytes; else use
                                the cached slice as today
```

The degraded signal travels: watch setup → per-root `FilePicker` atomic →
read once into the grep context → passed as a `bool` parameter into the read
path. The new identifier (`watch_coverage_degraded`) is consumed end-to-end:
written at setup, read in grep, branched on in the read path — no write-only
no-op.

## Files / functions to change

### 1. `crates/fff-core/src/file_picker.rs`
- Add a field to `FilePicker` (struct at `:434`):
  `watch_coverage_degraded: Arc<AtomicBool>` (default `false`). `Arc` so the
  watcher thread can hold a clone and set it without the picker write lock.
- Initialize it in the constructor(s) (`new_with_shared_state` path around
  `:698`).
- Add `pub(crate) fn watch_coverage_degraded(&self) -> bool` (relaxed load) and
  `pub(crate) fn watch_coverage_handle(&self) -> Arc<AtomicBool>` so the watcher
  can grab the handle when it spawns.
- Do NOT add this to any FFI / top-level public API surface (see breaking-change
  note). It stays `pub(crate)`.

### 2. `crates/fff-core/src/background_watcher.rs`
- In `BackgroundWatcher::new` / the debouncer-setup function that computes
  `aborted_early` (the NonRecursive branch, `:282`–`:326`): obtain the
  `Arc<AtomicBool>` handle from the picker (the function already reads the picker
  via `shared_picker_for_watching.read()` at `:286`) and, when `aborted_early`
  becomes true, `flag.store(true, Relaxed)`.
- Also set it on the macOS recursive-stream failure path if/when
  `MAX_CONSECUTIVE_WATCH_FAILURES`-style failures are detected (keep symmetric
  with Linux). If macOS currently has no such counter in this function, scope
  this to Linux for v1 and note the macOS extension as a follow-up — the read
  path backstop is platform-agnostic regardless.
- The flag is set holding only the picker READ lock (or via the cloned `Arc`,
  no lock at all) — no `stat`/I/O is performed while holding any lock here.

### 3. `crates/fff-core/src/grep.rs`
- In the grep entry that builds the per-search context (the `GrepCtx`/equivalent
  that carries `arena`, `base_path`, `budget`): read
  `picker.watch_coverage_degraded()` ONCE and store it as a `bool` on the
  context. This is the single atomic load per grep (not per file).
- At both `get_content_for_search` call sites (`:1266` and `:1718`) pass the new
  `recheck: bool` argument from the context.

### 4. `crates/fff-core/src/types.rs`
- Extend `get_content_for_search` (`:696`) with a `recheck: bool` parameter.
  - When `recheck == false`: behavior is byte-for-byte identical to today
    (persistent-cache fast path at `:709`, then existing branches). Zero added
    syscalls.
  - When `recheck == true`:
    1. `stat` the file once (`std::fs::metadata(abs)`).
    2. If `metadata.len() == self.size` AND `mtime == self.modified`
       (within the existing seconds granularity), use the existing cached/fast
       path unchanged.
    3. On mismatch, SKIP the persistent-cache slice and read fresh into the
       caller-owned `mmap_slot` (large files) or `buf` (small files) — the same
       transient-read machinery the budget-exhausted path already uses
       (`:721`–`:736`) — and return those fresh bytes. Bound the read by the
       FRESH `metadata.len()`, not `self.size`.
  - Crucially the refresh on the hot read path writes ONLY into the per-thread
    `mmap_slot`/`buf` (already `&mut` owned by the rayon worker), NOT into the
    shared `self.content` `OnceLock`. So `get_content_for_search` stays `&self`,
    no `&mut FileItem`, no write lock, no SIGBUS-on-shared-mmap risk. The stale
    `OnceLock` is reconciled lazily by the normal `handle_file_modify` path or a
    later sweep — the grep simply stops trusting it while degraded.
- This keeps the read path lock discipline intact: shared read lock only; the
  added work is a `stat` + a transient `mmap`/`read`, both of which the function
  already performs on its non-cached branches.

## Perf safeguards

- Healthy path (flag clear, the overwhelming common case): unchanged. One extra
  relaxed atomic load per grep call total; per-file behavior byte-identical to
  today, still zero syscalls on a cache hit.
- Degraded path: cost bounded to one `stat` per candidate that survives the
  bigram prefilter (the prefilter already runs before the heavy search). No
  recheck on files the prefilter rejected, because the recheck lives inside
  `get_content_for_search`, called only for survivors.
- Bound: at most one `stat` + at most one transient re-`open`/`mmap` per stale
  candidate per grep on a degraded root. No N-squared, no per-line syscalls.

## Locking plan (explicit)

- NO write lock on the grep hot path. The recheck runs under the existing
  shared READ lock.
- NO `Mutex`/`RwLock` is held across the `stat` or the transient file read in a
  way that differs from today — the function already opens/maps files under the
  read lock on its slow branches; the recheck adds a `stat` of the same nature.
- The watcher sets the degraded flag via an `Arc<AtomicBool>` (or under the read
  guard it already holds), performing no I/O under a lock.
- `FileItem` is NOT mutated on the read path (no `&mut`), so no lock upgrade is
  ever needed.

## Test strategy

Tests live in `crates/fff-core/tests/content_staleness_dropped_event.rs`
(currently on branch `worktree-agent-af6a745abea02a773`; port forward into this
branch when implementing). All run with `--no-default-features -p fff-search`.

1. **Promote the repro to a passing regression test.** Remove `#[ignore]` from
   `dropped_modify_event_leaves_stale_content`, set the picker's degraded flag
   before the grep (simulating the aborted watch), and assert the NEW identifier
   is now found. This is the core proof the truncation is healed.
2. **Same-size-edit case (new).** Build a target where the edit replaces an
   identifier with another of the SAME byte length (so `size` is unchanged),
   touch it so mtime advances, set the degraded flag, and assert the new
   identifier is found and the replaced one is gone. This guards against a
   regression into the Candidate-5 half-fix (length-only check).
3. **Degraded-vs-healthy gating test (new).** With the flag CLEAR, assert a
   dropped edit stays stale (documents that the healthy path is intentionally
   zero-syscall and does NOT self-heal) and that grep does the same number of
   file opens as before (or simply: new ident NOT found when flag clear, FOUND
   when flag set, same fixture). This proves the gate actually gates.
4. Keep `handled_modify_refreshes_content` and
   `dropped_event_small_file_bigram_only` green (no behavior change for them).

The tests must expose a way to set the degraded flag in-process. Add a
`pub(crate)`/`#[cfg(test)]` setter on `FilePicker`
(`set_watch_coverage_degraded(bool)`), or reuse the `Arc` handle. Do not widen
the public/FFI API for testing.

## Sequencing

1. Add the `watch_coverage_degraded` atomic + getters/handle on `FilePicker`
   (no behavior change yet). Build green.
2. Wire the watcher setup to set the flag on `aborted_early` (Linux). Build green.
3. Thread the `recheck` bool through `GrepCtx` and both call sites; implement the
   stat-gated branch in `get_content_for_search`. Build green.
4. Port forward + promote the regression test; add same-size and gating tests.
   Run `cargo test --no-default-features -p fff-search`.
5. (Optional, low-risk) Emit the "raise inotify watch limit" guidance when the
   flag is first set, reusing the existing `warn!` at the abort site.

## Non-goals

- Background reconciliation sweep (Candidate 3) — future layer, telemetry-gated.
- Changing the watch strategy / ENOSPC escalation (Candidate 4 beyond guidance).
- Eagerly refreshing the shared `OnceLock` on the read path — explicitly avoided
  to keep the path `&self` and lock-safe; lazy reconciliation is sufficient.
- macOS-specific degraded detection beyond the existing failure handling — note
  as follow-up if telemetry warrants; the read-path backstop is portable.

## Rollback / feature-gate

- The behavior is naturally gated: if the flag is never set, the code path is
  identical to today (zero-syscall). Rollback = stop setting the flag (one line
  in the watcher), which reverts to current behavior without touching the read
  path. Optionally guard the watcher's `flag.store(true,...)` behind a config
  bool (default on) so it can be disabled at runtime if a perf surprise appears
  on a degraded root.

## Breaking-change rule (honored)

Top-level Rust / Lua / C / Bun APIs MUST NOT change. The new
`watch_coverage_degraded` field, getters, handle, and the `recheck` parameter
are all internal (`pub(crate)` or private); the `get_content_for_search`
signature is `pub(crate)`, not part of the FFI surface. No exported function
signature, no Lua-facing API, and no C ABI changes. The grep results' shape is
unchanged — only their correctness on a degraded root improves.

---

## Top risk

Performing a `stat` per surviving candidate UNDER the shared picker read lock on
a degraded root: it is safe against deadlock (read lock is shared; no upgrade)
and matches the function's existing I/O-under-read-lock behavior, but on a very
large degraded root it adds real syscall latency to grep. Mitigations already in
the plan: the gate (healthy roots pay nothing), the prefilter narrowing the
candidate set before recheck, and the optional config kill-switch. If a degraded
root is also huge, this is still strictly better than silently-wrong results —
but it is the one place to watch.

## Human decision needed before implementation

One question: should the degraded flag be a one-way latch (set on first
`aborted_early`, never auto-cleared until a full rescan/reload) or
self-clearing? A latch is simpler and safe (worst case: a healed root keeps
paying the recheck cost until next rescan). Self-clearing requires defining
"coverage recovered," which the watcher cannot currently prove. Recommendation:
ship the one-way latch, clear it only on full rescan/reload. Confirm this is
acceptable before implementing.

## Glossary

- **FFI** — Foreign Function Interface. The Rust↔Lua/C boundary that must stay
  ABI-stable.
- **MCP** — Model Context Protocol. The connection that keeps a root "hot".
- **mmap** — memory-mapped file (`memmap2::Mmap`); the persistent content cache.
- **ENOSPC** — POSIX "no space left" errno; here, the inotify watch-limit error.
- **ABI** — Application Binary Interface.
