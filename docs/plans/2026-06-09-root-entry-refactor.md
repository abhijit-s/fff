# RootEntry Refactor — Track base_path Alongside Slug

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `Vec<String>` slug-only storage with `Vec<RootEntry { slug, base_path }>` so operators can see which filesystem path each fff-engine worker is managing, not just its blake3 hash.

**Architecture:** Introduce a `RootEntry` struct in `fff-ipc`, used by both `WorkerEntry` (persisted in `routing.json`) and `WorkerInfo` (master→ctl IPC response). The master receives `base_path` in every `Handshake` / `RouteInfo` request today and immediately throws it away after hashing — we change `assign_new_root` to store the canonical path alongside the slug. Backward compatibility for existing `routing.json` files is handled via a one-shot legacy migration on load: old `root_slugs: ["..."]` deserialize into a transient field and get hydrated into `roots: [RootEntry { slug, base_path: "" }]`, displayed as `<unknown>` until a fresh Handshake replaces them.

**Tech Stack:** Rust, serde (custom default + skip_serializing for legacy field migration), tokio, blake3 (existing).

---

## Scope & Non-Goals

**In scope:**
- New `RootEntry` type in `fff-ipc/src/routing.rs`.
- Rename `WorkerEntry.root_slugs` → `WorkerEntry.roots: Vec<RootEntry>`.
- Rename `WorkerInfo.root_slugs` → `WorkerInfo.roots: Vec<RootEntry>`.
- Helper methods on `WorkerEntry` for slug lookup/insert/remove to keep call sites tidy.
- Update master.rs (all 10 touch sites) to thread `base_path` into the new struct.
- Update fff-ctl print output to display human-readable paths.
- Backward-compat deserialization for `routing.json` written by pre-refactor masters.
- All existing tests updated; new tests for legacy migration and base_path round-trip.

**Out of scope:**
- Teaching workers to echo their base_paths back on adopt — first restart after upgrade will show `<unknown>` for adopted workers until clients reconnect. Acceptable trade-off; documented.
- New `fffctl roots` summary command (mentioned as optional in the discussion — left as a follow-up).
- Any change to the Lua/C/Bun top-level APIs (CLAUDE.md forbids it; this refactor doesn't touch them — `WorkerInfo`/`WorkerEntry` are internal IPC types between fff-engine binaries).

---

## File Structure

**Modified:**
- `crates/fff-ipc/src/routing.rs` — add `RootEntry`, change `WorkerEntry`, add helpers, legacy migration, update tests.
- `crates/fff-ipc/src/types.rs` — change `WorkerInfo`, update `root_count()`, update round-trip tests.
- `crates/fff-engine/src/master.rs` — `spawn_worker` initializer, `assign_new_root` push, `handle_evicted_root` removal, `collect_worker_info`/`worker_info` field copy, `Handshake` lookup.
- `crates/fff-ctl/src/main.rs` — `cmd_list_workers` and `cmd_worker_status` print loops.
- `crates/fff-engine/tests/integration.rs` — 4 sites constructing `WorkerEntry` literals.

**Created:** none.

**Touched but unchanged in shape:** `RoutingTable::entries_for_worker` keeps the same signature — its body reads `self.roots.len()` instead of `self.root_slugs.len()`.

---

## Design Decisions

### `RootEntry` shape

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootEntry {
    /// Blake3 hash of the canonical base_path (16 hex chars).
    pub slug: String,
    /// Canonical absolute base_path. Empty for entries migrated from
    /// legacy routing.json that predates this field — display as `<unknown>`.
    pub base_path: String,
}
```

Equality and `PartialEq` are derived so tests can assert vector contents naturally.

### `WorkerEntry` change

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEntry {
    pub index: u32,
    pub socket_path: String,
    pub pid: u32,
    #[serde(default)]
    pub roots: Vec<RootEntry>,
    /// LEGACY — read on load only, never written. Migrated into `roots`
    /// by `RoutingTable::load`. Kept private to prevent new code paths
    /// from touching it.
    #[serde(default, rename = "root_slugs", skip_serializing)]
    legacy_root_slugs: Vec<String>,
}

impl WorkerEntry {
    pub fn contains_slug(&self, slug: &str) -> bool {
        self.roots.iter().any(|r| r.slug == slug)
    }

    pub fn push_root(&mut self, slug: String, base_path: String) {
        self.roots.push(RootEntry { slug, base_path });
    }

    /// Returns true if a matching slug was removed.
    pub fn remove_slug(&mut self, slug: &str) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| r.slug != slug);
        before != self.roots.len()
    }

    /// Hydrate `roots` from `legacy_root_slugs` if a pre-refactor
    /// routing.json populated only the legacy field.
    fn migrate_legacy(&mut self) {
        if self.roots.is_empty() && !self.legacy_root_slugs.is_empty() {
            self.roots = std::mem::take(&mut self.legacy_root_slugs)
                .into_iter()
                .map(|slug| RootEntry { slug, base_path: String::new() })
                .collect();
        } else {
            // Drop any residual legacy data (shouldn't be both — defensive).
            self.legacy_root_slugs.clear();
        }
    }
}
```

`skip_serializing` (not `skip_serializing_if`) guarantees the field is never written. `#[serde(default)]` makes missing-on-load OK once new code writes a routing.json that lacks the legacy key.

### `WorkerInfo` change

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub index: u32,
    pub socket_path: String,
    pub roots: Vec<RootEntry>,
    pub pid: u32,
}

impl WorkerInfo {
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}
```

No legacy field here — `WorkerInfo` is an in-flight IPC message between master and ctl built from the same repo. The fff-ipc bump is coordinated with the master/ctl rebuild.

### Where base_path is sourced

In `master.rs::assign_new_root(base_path: &str)`, the canonical path is computed once via `std::fs::canonicalize(base_path).unwrap_or(base_path)` and stored in the `RootEntry`. This matches what `base_path_slug` canonicalizes for hashing, so the displayed path is consistent with how the slug was derived.

---

## Task 1: Add `RootEntry` and migrate `WorkerEntry` (TDD)

**Files:**
- Modify: `crates/fff-ipc/src/routing.rs`
- Test: `crates/fff-ipc/src/routing.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write failing test for `RootEntry` round-trip and legacy migration**

Add to `routing.rs` tests:

```rust
#[test]
fn root_entry_serializes_as_object() {
    let r = RootEntry {
        slug: "abc123".into(),
        base_path: "/home/me/proj".into(),
    };
    let json = serde_json::to_string(&r).unwrap();
    assert!(json.contains("\"slug\":\"abc123\""));
    assert!(json.contains("\"base_path\":\"/home/me/proj\""));
}

#[test]
fn worker_entry_round_trips_with_roots() {
    let mut rt = RoutingTable::default();
    let mut entry = WorkerEntry {
        index: 0,
        socket_path: "worker-0.sock".into(),
        pid: 1000,
        roots: vec![
            RootEntry { slug: "s1".into(), base_path: "/a".into() },
            RootEntry { slug: "s2".into(), base_path: "/b".into() },
        ],
        legacy_root_slugs: vec![],
    };
    // Ensure migrate_legacy is idempotent on already-new entries.
    entry.migrate_legacy();
    rt.workers.insert(0, entry);

    let json = serde_json::to_string(&rt).unwrap();
    assert!(!json.contains("root_slugs"), "legacy key must not be written");

    let rt2: RoutingTable = serde_json::from_str(&json).unwrap();
    assert_eq!(rt2.workers[&0].roots[0].base_path, "/a");
    assert_eq!(rt2.workers[&0].roots[1].slug, "s2");
}

#[test]
fn load_migrates_legacy_root_slugs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("routing.json");
    // Hand-craft a pre-refactor routing.json with the old shape.
    let legacy_json = r#"{
        "ring_state": { "nodes": [] },
        "workers": {
            "0": {
                "index": 0,
                "socket_path": "worker-0.sock",
                "pid": 1000,
                "root_slugs": ["legacyslug1", "legacyslug2"]
            }
        }
    }"#;
    std::fs::write(&path, legacy_json).unwrap();

    let rt = RoutingTable::load(&path).unwrap();
    let entry = &rt.workers[&0];
    assert_eq!(entry.roots.len(), 2);
    assert_eq!(entry.roots[0].slug, "legacyslug1");
    assert_eq!(entry.roots[0].base_path, "", "legacy entries hydrate with empty base_path");
    assert_eq!(entry.roots[1].slug, "legacyslug2");
}
```

Also update the existing `routing_table_round_trips_json`, `entries_for_worker_counts_slugs`, `save_and_load_round_trip`, and `save_is_atomic_no_partial_file` tests to use `roots: Vec<RootEntry>` and add `legacy_root_slugs: vec![]` to literals. Update `make_entry` helper:

```rust
fn make_entry(index: u32, slugs: &[&str]) -> WorkerEntry {
    WorkerEntry {
        index,
        socket_path: format!("worker-{index}.sock"),
        pid: 1000 + index,
        roots: slugs.iter().map(|s| RootEntry {
            slug: (*s).to_string(),
            base_path: format!("/test/{s}"),
        }).collect(),
        legacy_root_slugs: vec![],
    }
}
```

Update assertions accordingly:
- `assert_eq!(rt2.workers[&0].root_slugs, vec!["abc", "def"]);` → `assert_eq!(rt2.workers[&0].roots.iter().map(|r| r.slug.as_str()).collect::<Vec<_>>(), vec!["abc", "def"]);`
- `assert_eq!(rt2.workers[&0].root_slugs, vec!["slug1"]);` → same pattern.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p fff-ipc routing`
Expected: FAIL — `RootEntry` not defined, `roots` field missing.

- [ ] **Step 3: Implement `RootEntry`, new `WorkerEntry`, helpers, and migration**

Replace the existing `WorkerEntry` definition (lines 17–24) with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootEntry {
    pub slug: String,
    pub base_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEntry {
    pub index: u32,
    pub socket_path: String,
    pub pid: u32,
    #[serde(default)]
    pub roots: Vec<RootEntry>,
    #[serde(default, rename = "root_slugs", skip_serializing)]
    legacy_root_slugs: Vec<String>,
}

impl WorkerEntry {
    pub fn contains_slug(&self, slug: &str) -> bool {
        self.roots.iter().any(|r| r.slug == slug)
    }

    pub fn push_root(&mut self, slug: String, base_path: String) {
        self.roots.push(RootEntry { slug, base_path });
    }

    pub fn remove_slug(&mut self, slug: &str) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| r.slug != slug);
        before != self.roots.len()
    }

    fn migrate_legacy(&mut self) {
        if self.roots.is_empty() && !self.legacy_root_slugs.is_empty() {
            self.roots = std::mem::take(&mut self.legacy_root_slugs)
                .into_iter()
                .map(|slug| RootEntry { slug, base_path: String::new() })
                .collect();
        } else {
            self.legacy_root_slugs.clear();
        }
    }
}
```

Update `RoutingTable::entries_for_worker` (line 38) to read from `roots`:

```rust
pub fn entries_for_worker(&self, worker_index: u32) -> usize {
    self.workers
        .get(&worker_index)
        .map(|e| e.roots.len())
        .unwrap_or(0)
}
```

Update `RoutingTable::load` (line 51) to run migration after deserialization:

```rust
pub fn load(path: &Path) -> io::Result<Self> {
    match fs::read_to_string(path) {
        Ok(s) => {
            let mut table: Self = serde_json::from_str(&s)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            for entry in table.workers.values_mut() {
                entry.migrate_legacy();
            }
            Ok(table)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
        Err(e) => Err(e),
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p fff-ipc routing`
Expected: PASS, including the new legacy migration test.

- [ ] **Step 5: Commit**

```bash
git add crates/fff-ipc/src/routing.rs
git commit -m "feat(fff-ipc): introduce RootEntry to track base_path with slug

Replace WorkerEntry.root_slugs: Vec<String> with roots: Vec<RootEntry>
so master state carries the canonical path that hashed to each slug.
Preserve compat with pre-refactor routing.json via a private legacy
field hydrated on load."
```

---

## Task 2: Migrate `WorkerInfo` to use `RootEntry`

**Files:**
- Modify: `crates/fff-ipc/src/types.rs:40-51`
- Test: `crates/fff-ipc/src/types.rs` (existing inline tests)

- [ ] **Step 1: Write failing test for `WorkerInfo` round-trip with roots**

Update the existing round-trip tests in `types.rs` (around lines 416 and 475). Replace assertions like:

```rust
WorkerInfo {
    index: 0,
    socket_path: "s".into(),
    root_slugs: vec!["abc".into()],
    pid: 100,
},
```

with:

```rust
WorkerInfo {
    index: 0,
    socket_path: "s".into(),
    roots: vec![RootEntry {
        slug: "abc".into(),
        base_path: "/proj/a".into(),
    }],
    pid: 100,
},
```

Add a new assertion that confirms the path round-trips:

```rust
assert_eq!(workers[0].roots[0].base_path, "/proj/a");
```

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p fff-ipc types`
Expected: FAIL — `root_slugs` no longer defined, `roots` field doesn't exist.

- [ ] **Step 3: Implement `WorkerInfo` change**

Add import at top of `types.rs`:

```rust
use crate::routing::RootEntry;
```

Replace `WorkerInfo` (lines 40-51) with:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub index: u32,
    pub socket_path: String,
    pub roots: Vec<RootEntry>,
    pub pid: u32,
}

impl WorkerInfo {
    pub fn root_count(&self) -> usize {
        self.roots.len()
    }
}
```

- [ ] **Step 4: Run tests to verify pass**

Run: `cargo test -p fff-ipc`
Expected: PASS for all `fff-ipc` tests.

- [ ] **Step 5: Commit**

```bash
git add crates/fff-ipc/src/types.rs
git commit -m "feat(fff-ipc): WorkerInfo carries RootEntry instead of slug strings

Master→ctl IPC now surfaces base_path alongside each slug so operator
tools can display human-readable paths."
```

---

## Task 3: Thread `base_path` through master.rs

**Files:**
- Modify: `crates/fff-engine/src/master.rs` (10 sites: 127, 154, 165, 218-219, 260, 267-268, 609)

- [ ] **Step 1: Update `spawn_worker` initializer (around line 127)**

Replace:

```rust
routing.workers.insert(
    index,
    WorkerEntry {
        index,
        socket_path: socket.to_string_lossy().into(),
        pid,
        root_slugs: vec![],
    },
);
```

with:

```rust
routing.workers.insert(
    index,
    WorkerEntry {
        index,
        socket_path: socket.to_string_lossy().into(),
        pid,
        roots: vec![],
        legacy_root_slugs: vec![],
    },
);
```

Note: `legacy_root_slugs` is private to `fff-ipc`, so this won't compile from `master.rs`. Two options:
- Add a constructor `WorkerEntry::new(index, socket_path, pid)` to `fff-ipc` that hides the legacy field.
- Or make `legacy_root_slugs` `pub` and accept the leak.

**Decision: add `WorkerEntry::new` constructor.** Cleaner, hides the legacy field permanently. Add to `routing.rs`:

```rust
impl WorkerEntry {
    pub fn new(index: u32, socket_path: String, pid: u32) -> Self {
        Self {
            index,
            socket_path,
            pid,
            roots: Vec::new(),
            legacy_root_slugs: Vec::new(),
        }
    }
    // existing helpers...
}
```

Then in master.rs:

```rust
routing.workers.insert(
    index,
    WorkerEntry::new(index, socket.to_string_lossy().into(), pid),
);
```

(Apply the same constructor in tests/literals — see Task 5.)

- [ ] **Step 2: Update `collect_worker_info` (line 146)**

Replace:

```rust
.map(|e| WorkerInfo {
    index: e.index,
    socket_path: e.socket_path.clone(),
    root_slugs: e.root_slugs.clone(),
    pid: e.pid,
})
```

with:

```rust
.map(|e| WorkerInfo {
    index: e.index,
    socket_path: e.socket_path.clone(),
    roots: e.roots.clone(),
    pid: e.pid,
})
```

- [ ] **Step 3: Update `worker_info` (line 160)**

Replace:

```rust
routing.workers.get(&index).map(|e| WorkerInfo {
    index: e.index,
    socket_path: e.socket_path.clone(),
    root_slugs: e.root_slugs.clone(),
    pid: e.pid,
})
```

with:

```rust
routing.workers.get(&index).map(|e| WorkerInfo {
    index: e.index,
    socket_path: e.socket_path.clone(),
    roots: e.roots.clone(),
    pid: e.pid,
})
```

- [ ] **Step 4: Update `handle_evicted_root` (lines 214-225)**

Replace:

```rust
async fn handle_evicted_root(&self, slug: &str) {
    let mut routing = self.routing.lock().await;
    let now = Instant::now();
    for (&idx, entry) in routing.workers.iter_mut() {
        entry.root_slugs.retain(|s| s != slug);
        if entry.root_slugs.is_empty() {
            self.idle_since.lock().await.entry(idx).or_insert(now);
        }
    }
    self.persist_routing(&routing);
    tracing::debug!("master: routing entry removed for evicted slug {slug}");
}
```

with:

```rust
async fn handle_evicted_root(&self, slug: &str) {
    let mut routing = self.routing.lock().await;
    let now = Instant::now();
    for (&idx, entry) in routing.workers.iter_mut() {
        entry.remove_slug(slug);
        if entry.roots.is_empty() {
            self.idle_since.lock().await.entry(idx).or_insert(now);
        }
    }
    self.persist_routing(&routing);
    tracing::debug!("master: routing entry removed for evicted slug {slug}");
}
```

- [ ] **Step 5: Update `assign_new_root` (lines 254-272)**

Compute the canonical base_path once at the top of the write-locked section, so the stored path matches what `base_path_slug` hashed. Replace:

```rust
let should_scale_out = {
    let mut routing = self.routing.lock().await;

    // Re-check after lock: a concurrent Handshake may have added this slug already.
    for (idx, entry) in &routing.workers {
        if entry.root_slugs.contains(&slug) {
            return Some(*idx);
        }
    }

    let mut scale_out = false;
    if let Some(entry) = routing.workers.get_mut(&index) {
        entry.root_slugs.push(slug.clone());
        let load = entry.root_slugs.len() as u32;
        let total_workers = routing.workers.len() as u32;
        scale_out =
            load >= self.config.roots_per_worker_max && total_workers < self.config.n_max;
    }
    self.idle_since.lock().await.remove(&index);
    self.persist_routing(&routing);
    scale_out
};
```

with:

```rust
let canonical = std::fs::canonicalize(base_path)
    .map(|p| p.to_string_lossy().into_owned())
    .unwrap_or_else(|_| base_path.to_string());

let should_scale_out = {
    let mut routing = self.routing.lock().await;

    // Re-check after lock: a concurrent Handshake may have added this slug already.
    for (idx, entry) in &routing.workers {
        if entry.contains_slug(&slug) {
            return Some(*idx);
        }
    }

    let mut scale_out = false;
    if let Some(entry) = routing.workers.get_mut(&index) {
        entry.push_root(slug.clone(), canonical);
        let load = entry.roots.len() as u32;
        let total_workers = routing.workers.len() as u32;
        scale_out =
            load >= self.config.roots_per_worker_max && total_workers < self.config.n_max;
    }
    self.idle_since.lock().await.remove(&index);
    self.persist_routing(&routing);
    scale_out
};
```

- [ ] **Step 6: Update `Handshake` routing hit (lines 605-615)**

Replace:

```rust
let routing_hit = {
    let routing = ms.routing.lock().await;
    routing.workers.iter().find_map(|(&idx, e)| {
        if e.root_slugs.contains(&slug) {
            Some(idx)
        } else {
            None
        }
    })
};
```

with:

```rust
let routing_hit = {
    let routing = ms.routing.lock().await;
    routing.workers.iter().find_map(|(&idx, e)| {
        if e.contains_slug(&slug) {
            Some(idx)
        } else {
            None
        }
    })
};
```

- [ ] **Step 7: Build and verify type checks**

Run: `cargo check -p fff-engine`
Expected: PASS, zero errors.

- [ ] **Step 8: Run engine unit tests**

Run: `cargo test -p fff-engine --lib`
Expected: PASS for all unit tests (integration tests addressed in Task 5).

- [ ] **Step 9: Commit**

```bash
git add crates/fff-ipc/src/routing.rs crates/fff-engine/src/master.rs
git commit -m "feat(fff-engine): persist base_path in routing table

assign_new_root canonicalizes the incoming path and stores it alongside
the slug. handle_evicted_root uses the new remove_slug helper. Worker
info responses surface the path to operator tools."
```

---

## Task 4: Update fff-ctl output to show paths

**Files:**
- Modify: `crates/fff-ctl/src/main.rs:140-171, 252-280`

- [ ] **Step 1: Update `cmd_list_workers` (line 140)**

Replace the inner print loop:

```rust
for w in &workers {
    println!(
        "{:<6}  {:<7}  {:<8}  {}",
        w.index,
        w.pid,
        w.root_count(),
        w.socket_path
    );
    for slug in &w.root_slugs {
        println!("       slug: {slug}");
    }
}
```

with:

```rust
for w in &workers {
    println!(
        "{:<6}  {:<7}  {:<8}  {}",
        w.index,
        w.pid,
        w.root_count(),
        w.socket_path
    );
    for root in &w.roots {
        let path_display = if root.base_path.is_empty() {
            "<unknown>"
        } else {
            root.base_path.as_str()
        };
        println!("       {path_display}  (slug: {})", root.slug);
    }
}
```

- [ ] **Step 2: Update `cmd_worker_status` (line 262)**

Replace:

```rust
for slug in &info.root_slugs {
    println!("  slug: {slug}");
}
```

with:

```rust
for root in &info.roots {
    let path_display = if root.base_path.is_empty() {
        "<unknown>"
    } else {
        root.base_path.as_str()
    };
    println!("  {path_display}  (slug: {})", root.slug);
}
```

- [ ] **Step 3: Build and lint**

Run: `cargo check -p fff-ctl && cargo clippy -p fff-ctl --no-deps`
Expected: PASS, zero warnings.

- [ ] **Step 4: Manually verify ctl print formatting compiles & runs end-to-end**

Run: `cargo run -p fff-ctl -- list-workers || true`
Expected: Either prints "master not running" (no engine started) or shows worker rows with the new `<base_path>  (slug: …)` format. Exit code may be non-zero if master is absent; that's fine for this step.

- [ ] **Step 5: Commit**

```bash
git add crates/fff-ctl/src/main.rs
git commit -m "feat(fff-ctl): display base_path alongside slug in worker listings

list-workers and worker-status now print human-readable paths managed
by each worker. Entries migrated from pre-refactor routing.json display
as <unknown> until the client reconnects with a fresh Handshake."
```

---

## Task 5: Update integration tests

**Files:**
- Modify: `crates/fff-engine/tests/integration.rs:545, 728, 739, 819, 828`

- [ ] **Step 1: Update WorkerEntry literal at line 545**

Search context: find the test constructing `WorkerEntry { ... root_slugs: vec!["some-slug".into()], ... }`.

Replace with:

```rust
WorkerEntry {
    index: /* keep existing */,
    socket_path: /* keep existing */,
    pid: /* keep existing */,
    roots: vec![RootEntry {
        slug: "some-slug".into(),
        base_path: "/test/proj".into(),
    }],
    legacy_root_slugs: vec![],
}
```

Same pattern for line 819 (`root_slugs: vec![]` → `roots: vec![], legacy_root_slugs: vec![]`) and line 828 (`root_slugs: vec!["stale-slug".into()]` → `roots: vec![RootEntry { slug: "stale-slug".into(), base_path: "/test/stale".into() }], legacy_root_slugs: vec![]`).

Note: `legacy_root_slugs` is private. Two options:
- If these tests live in the `fff-engine` crate (they do — `tests/integration.rs`), they cannot construct the private field. **Use `WorkerEntry::new(idx, socket, pid)` followed by `entry.push_root(slug, base_path)`** to populate.

Concretely:

```rust
let mut entry = WorkerEntry::new(0, "worker-0.sock".into(), 1000);
entry.push_root("some-slug".into(), "/test/proj".into());
```

Apply this transform at all 3 sites (545, 819, 828).

- [ ] **Step 2: Update slug-counting assertions at lines 728 and 739**

Replace:

```rust
let total_slugs1: usize = table1.workers.values().map(|e| e.root_slugs.len()).sum();
```

with:

```rust
let total_slugs1: usize = table1.workers.values().map(|e| e.roots.len()).sum();
```

Apply the same transform at line 739 for `total_slugs2`.

- [ ] **Step 3: Add `RootEntry` import if needed**

At the top of `integration.rs`, ensure `use fff_ipc::routing::{RootEntry, RoutingTable, WorkerEntry};` (or whatever the existing import path is — extend it to include `RootEntry`). If the test file already imports `WorkerEntry` via a glob, no change needed.

- [ ] **Step 4: Run integration tests**

Run: `cargo test -p fff-engine --test integration`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/fff-engine/tests/integration.rs
git commit -m "test(fff-engine): update integration tests for RootEntry

Construct WorkerEntry via the new constructor and push_root helper;
slug-counting assertions read from the roots vec."
```

---

## Task 6: Full-workspace verification

**Files:** none modified — verification only.

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --workspace`
Expected: PASS across all crates. Pay attention to any leftover `root_slugs` references the compiler missed (there shouldn't be — `root_slugs` is now a private field, so any external usage breaks compilation).

- [ ] **Step 2: Run lints**

Run: `make lint`
Expected: clippy passes with no warnings.

- [ ] **Step 3: Format**

Run: `make format`
Expected: no diff (everything already formatted) or formatting applied cleanly.

- [ ] **Step 4: Manual end-to-end smoke test**

Steps:
1. Build: `make build`
2. Start a fresh engine in one terminal — let it pick up two distinct project paths via the Neovim picker or MCP client (or directly with `fff-engine --base-path /path/to/repoA` then another for `/path/to/repoB`).
3. Run: `./target/debug/fffctl list-workers`

Expected output shape:
```
INDEX  PID    ROOTS  SOCKET
0      12345  2      /var/folders/.../fff/worker-0.sock
       /Users/a.salvi/path/to/repoA  (slug: 0123456789abcdef)
       /Users/a.salvi/path/to/repoB  (slug: fedcba9876543210)
```

4. Run: `./target/debug/fffctl worker-status 0`

Expected output:
```
worker-0: pid=12345 roots=2
  socket: /var/folders/.../fff/worker-0.sock
  /Users/a.salvi/path/to/repoA  (slug: 0123456789abcdef)
  /Users/a.salvi/path/to/repoB  (slug: fedcba9876543210)
```

5. Run: `./target/debug/fffctl status /Users/a.salvi/path/to/repoA`

Expected output:
```
Route for /Users/a.salvi/path/to/repoA: worker-0 (pid=12345, roots=2)
  socket: /var/folders/.../fff/worker-0.sock
```

- [ ] **Step 5: Manual backward-compat smoke**

Steps:
1. Stop the engine: `./target/debug/fffctl stop --all`.
2. Locate `routing.json`: `./target/debug/fffctl paths .` and read the routing.json line.
3. Hand-edit that file to convert one worker entry back to the legacy shape (replace `"roots": [{"slug": "X", "base_path": "/Y"}]` with `"root_slugs": ["X"]`).
4. Restart the engine; it should adopt the prior workers from routing.json.
5. Run: `./target/debug/fffctl list-workers`

Expected: the legacy entry shows `<unknown>  (slug: X)`. New entries created by fresh Handshakes still show their base_path.

- [ ] **Step 6: Verify no `root_slugs` references remain in production code**

Run: `rg "root_slugs" crates/`
Expected: ONLY references inside `fff-ipc/src/routing.rs` (the private `legacy_root_slugs` field and its rename attribute, plus the migration logic). Zero references anywhere else.

- [ ] **Step 7: Final commit**

If any tweaks were needed during smoke testing:

```bash
git add -p   # review each hunk
git commit -m "chore: post-smoke-test polish for RootEntry refactor"
```

Otherwise nothing to commit — the refactor is done.

---

## Risks & Open Questions

**Risk: routing.json key collision.** If a future contributor adds a new `root_slugs` field for a different purpose, they'd clash with the `rename = "root_slugs"` migration. Mitigated by keeping the legacy field private and documented as "do not touch."

**Risk: legacy `<unknown>` entries persist forever if a client never re-Handshakes.** Acceptable — `<unknown>` is informational; routing still works (it's keyed on slug). Could add a future enhancement: when displaying, look up the slug in the worker's per-root frecency directory and read back a `base_path.txt` if we ever start storing one. Out of scope here.

**Risk: canonicalize on a no-longer-existing path returns the input unchanged.** Already the behavior of `base_path_slug` (`paths.rs:44`). Symmetry preserved.

**Open question: should `WorkerEntry::new` be the only constructor?** Currently the field literal form is still possible from within `fff-ipc` (and from tests that import the legacy field). The plan keeps `roots` public for ergonomics. If a future change wants stricter encapsulation, add a builder; not needed now.

---

## Self-Review

**Spec coverage:**
- ✅ Introduce `RootEntry` — Task 1.
- ✅ Rename `WorkerEntry.root_slugs` → `roots` — Task 1.
- ✅ Rename `WorkerInfo.root_slugs` → `roots` — Task 2.
- ✅ Thread base_path through master — Task 3.
- ✅ Display in fff-ctl — Task 4.
- ✅ Backward compat for routing.json — Task 1 (legacy field + migrate_legacy + load hook).
- ✅ Tests updated — Tasks 1, 2, 5.
- ✅ Manual smoke — Task 6.

**Placeholder scan:** None of the forbidden phrases ("TBD", "implement later", "similar to Task N") appear. Every code step has the full code shown.

**Type consistency:**
- `RootEntry { slug, base_path }` — used identically in routing.rs, types.rs, master.rs, ctl, tests.
- `WorkerEntry::new(index, socket_path, pid)` — defined in Task 3 Step 1, used in master.rs and integration tests (Task 5).
- `contains_slug`, `push_root`, `remove_slug`, `migrate_legacy` — all defined in Task 1, referenced consistently in Tasks 3 and 5.
- `root_count()` — kept on `WorkerInfo`, body updated in Task 2 to read `self.roots.len()`. Used unchanged in fff-ctl (Task 4 still calls `w.root_count()`).
