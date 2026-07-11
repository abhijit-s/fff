# TODO

## Versioned protocol — follow-up increments
Context: the JSON wire protocol ships alongside legacy bincode via dual-read (ADR-002, `crates/fff-ipc/PROTOCOL.md`). These finish the migration.

- [ ] **Migrate `fffctl` to the versioned JSON protocol.** It still speaks legacy bincode (`MasterRequest`); the dual-read engine serves it unchanged for now. Move it onto `fff-ipc`'s JSON envelope so every first-party client speaks one wire format.
- [ ] **Remove the legacy bincode path.** Only after all clients — editor, `fffctl`, `fff-mcp` — have aged onto JSON and no old daemons remain. Drop the bincode dispatch arms, the `read_frame`/sniff dual-read branch, and the "append variants last" ordinal discipline.

## Upstream-merge follow-ups
Context: merge `3fc923c` brought upstream `dmtrKovalenko/fff` into the fork (adopt upstream structure, retrofit daemon on top). These items were flagged during that merge and deferred.

- [ ] **Review the `background_watcher.rs` watch-loop retrofit.** Upstream inlined the multi-directory watch loop; the merge re-extracted it into `watch_all_dirs()` so the fork's self-clearing watch-coverage recovery (`watch_coverage_handle()` + the new-directory recovery block) can call it from two sites. Behavior is identical to upstream's inline version — eyeball the diff and confirm no watcher logic drifted.
- [ ] **Pin zig 0.16 for the `zlob` build in CI.** Vendored zlob 1.6.1 uses newer Zig APIs (`std.Io.Threaded`/`signal_stack_size`) that fail on `zig@0.15`. `make build` / release `zlob` builds need `zig 0.16.0`. Verify the release workflow's Zig version and bump it, else the prebuilt `zlob` artifacts fail.
- [ ] **Reconcile `fff-python` vs `fff-client`.** The merge pulled in upstream's `fff-python` (pyo3 cdylib bindings) alongside the fork's own `fff-client` (deps-light Python client) — two Python surfaces now coexist. Decide which to keep and prune the other (workspace member, release/CI wheels, docs).
