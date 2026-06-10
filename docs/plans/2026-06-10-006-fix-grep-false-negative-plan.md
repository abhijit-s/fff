# Grep content-mode false negative for `McpConfig` — investigation

Date: 2026-06-10
Branch: worktree off main @ ddc7cd3

## Summary

REPRODUCED: **No** — not at any layer reachable by an in-process controlled
test (grep core, engine handlers, query parser, option mapping). The reported
live MCP (Model Context Protocol) symptom — content-mode `grep "McpConfig"`
returning `0 matches` while `files_with_matches`, `multi_grep`, and the
trailing-space variant returned results — could not be made to occur in a test
that drives the same code path with the same inputs and the same index state.

Per the task's own instruction ("If you cannot reproduce in a controlled test,
say so with evidence — do not force a fix"), no behavioural change was made to
`grep.rs`. Two permanent regression tests were added that pin the correct
behaviour so any future regression in this path fails loudly.

## What the matrix claims

| query | mode | live result |
|-------|------|-------------|
| `McpConfig` | content (default) | 0 (BUG) |
| `McpConfig` | files_with_matches | many |
| `McpConfig ` (trailing space) | content | 47 |
| `multi_grep ["McpConfig"]` | content | 47 |
| `McpConfigError` / `FffConfig` / `McpRoot` / `RootRegistry` | content | non-zero |

## Code path that the matrix points at

- MCP single `grep` (content) → `FffServer::grep` →
  `proxy_grep` (Unix, when an fff-engine pool exists) → IPC `SearchRequest::Grep`
  → engine `handle_grep` → `picker.grep(parsed, options)` → `grep_search`
  (PlainText) in `crates/fff-core/src/grep.rs`.
- MCP `files_with_matches` takes the identical path; the ONLY option difference
  produced by `make_grep_options` is `max_matches_per_file` (content = 10,
  files_with_matches = 1). `page_limit` (50), `after_context` (8),
  `classify_definitions` (true), `trim_whitespace` (true) are identical.
- `multi_grep` → `multi_grep_search` (Aho-Corasick, no memmem prefilter).
- The auto-fuzzy fallback and auto-broaden retry live ONLY in the LOCAL
  `perform_grep` (server.rs), NOT in `proxy_grep`. The proxy returns the raw
  engine result and prints literally `"0 matches."` when `wire.matches` is empty.
  So a live `0 matches` means the engine returned an empty match set.
- At the engine, the raw query string difference (`"McpConfig"` vs
  `"McpConfig "`) is erased: `QueryParser::parse` calls `query.trim()` first, so
  both parse to the identical `FuzzyQuery::Text("McpConfig")`.

## Controlled reproduction attempts (all FAILED to reproduce)

Added test `test_grep_real_repo_bare_query_content_match` that:
1. Indexes the actual repo root (the known-reproducing corpus).
2. Builds a REAL bigram index via `build_bigram_index` and attaches it
   (`set_bigram_index` also creates the live `BigramOverlay`).
3. Uses `FFFMode::Ai` + `enable_content_indexing` to match the engine exactly.
4. Uses the exact `make_grep_options` content (max 10/file) and
   files_with_matches (max 1/file) options.
5. Runs the full discriminator matrix.

Result on this repo (HEAD ddc7cd3):

```
McpConfig: content(max10)=31  trailing=31  files_with_matches=5  multi_grep=31
controls FffConfig / McpConfigError / McpRoot / RootRegistry: all >= 1
```

Content mode returns 31 matching lines — NOT zero. content == trailing ==
multi_grep. files_with_matches=5 corresponds to the "many files" in the matrix
(it counts files, not lines). Every documented single-grep-only component was
exercised and behaved correctly: `AiGrepConfig` parse, the literal memmem
whole-file prefilter, and the literal-bigram `idx.query` prefilter.

A smaller synthetic corpus (no bigram index) was also tested
(`test_grep_bare_pascalcase_content_match`) and likewise returns the correct
non-zero count.

## Conclusion on root cause

The defect is not present in `grep.rs`, the engine handlers, the wire option
mapping, or the query parser on this repo state. The factors that remain
exclusive to the live process and were NOT reproducible in-process:

- A transient FRESH-INDEX race: the engine answers a grep before
  `run_post_scan` finishes building the bigram index/overlay, or while the
  content cache / `set_binary` classification pass is still mutating
  `FileItem`s. A half-built index could under-populate candidates for one query
  while a slightly-later query (trailing-space retry, or a subsequent
  `multi_grep`/`files_with_matches` call) sees the completed index. This is
  timing-dependent and not deterministically reproducible from a fully-built
  in-process index.
- The live engine's frecency DB changes prefilter sort order, which interacts
  with `page_limit`; but since `perform_grep` scans all candidates when the page
  never fills, sort order cannot turn a real match into zero.

Given the evidence, forcing a code change into `grep.rs` would be speculative
and risk the grep hot path. No change was made beyond the regression tests.

## Conditions under which the live repro would hold

Re-test against a genuinely FRESH engine index (immediately after the engine
starts indexing this repo, before `run_post_scan`/bigram build completes),
issuing the single content grep first. If `0 matches` reproduces only on the
first call after a cold start and disappears on retry, the fix belongs in the
index-readiness gating (ensure `grep_search` treats a not-yet-ready bigram index
as "no prefilter / scan all" — which `idx.is_ready()` already intends) or in the
engine deferring grep until the initial post-scan completes.

## Changes made

- `crates/fff-core/src/grep.rs`: added two non-ignored regression tests plus
  shared helpers (`mcp_grep_options`, `grep_content_count`, `grep_files_count`)
  that mirror the MCP option split and assert the bare PascalCase query matches
  in content mode, equals the trailing-space variant, and is consistent with
  `files_with_matches` and `multi_grep`.

## Verification

- `cargo build --no-default-features -p fff-search -p fff-engine -p fff-ipc` — ok
- `cargo test --no-default-features -p fff-search -p fff-engine` — all pass
  (fff-search lib: 94 passed incl. the 2 new tests)
- `cargo clippy --no-default-features -p fff-search --tests` — no new warnings
  in grep.rs (pre-existing warnings only in unrelated integration test files)
- `cargo fmt` — applied
