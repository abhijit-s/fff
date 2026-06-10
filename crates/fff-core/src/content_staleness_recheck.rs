//! Regression tests for the degraded-root-gated content recheck.
//!
//! On a hot root a dropped filesystem-watcher event leaves `FileItem.size` and
//! the persistent mmap cache stale, so grep silently truncates appended/changed
//! content. The fix re-stats cached candidates ONLY when the root's
//! `watch_coverage_degraded` flag is set; healthy roots keep the zero-syscall
//! fast path. These tests drive `FilePicker` directly with `watch: false` so
//! events are delivered/withheld by hand.

use std::fs;
use std::time::Duration;
use tempfile::TempDir;

use crate::file_picker::{FFFMode, FilePicker};
use crate::grep::{GrepMode, GrepSearchOptions, parse_grep_query};
use crate::{FilePickerOptions, SharedFilePicker, SharedFrecency};

const OLD_IDENT: &str = "RootRegistry";
const NEW_IDENT: &str = "McpConfig";

/// Dense corpus so the bigram index keeps filtering power; each filler file is
/// above MMAP_THRESHOLD so the content cache is populated.
fn seed_corpus(base: &std::path::Path, count: usize) {
    let filler_line = "pub fn handler() { let registry = RootRegistry::new(); }\n";
    let body: String = filler_line.repeat(400); // ~22 KB, above MMAP_THRESHOLD
    for i in 0..count {
        fs::write(base.join(format!("mod_{i:03}.rs")), &body).unwrap();
    }
}

/// Large enough to be mmap-cached, contains OLD_IDENT but not NEW_IDENT.
fn target_initial() -> String {
    let mut s = String::new();
    s.push_str("pub struct RootRegistry {}\npub fn build_roots() {}\n");
    s.push_str(&"pub fn helper() { let x = RootRegistry::new(); }\n".repeat(400));
    s
}

/// Append NEW_IDENT — grows the file (size + mtime change).
fn target_modified() -> String {
    let mut s = target_initial();
    s.push_str("pub struct McpConfig { pub root: String }\n");
    s
}

fn boot(base: &std::path::Path) -> (SharedFilePicker, SharedFrecency) {
    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        FilePickerOptions {
            base_path: base.to_string_lossy().to_string(),
            enable_mmap_cache: true,
            enable_content_indexing: true,
            mode: FFFMode::Neovim,
            watch: false,
            ..Default::default()
        },
    )
    .expect("Failed to create FilePicker");

    wait_for_bigram(&shared_picker);
    (shared_picker, shared_frecency)
}

fn grep_opts() -> GrepSearchOptions {
    GrepSearchOptions {
        max_file_size: 10 * 1024 * 1024,
        max_matches_per_file: 200,
        smart_case: true,
        file_offset: 0,
        page_limit: 200,
        mode: GrepMode::PlainText,
        time_budget_ms: 0,
        before_context: 0,
        after_context: 0,
        classify_definitions: false,
        trim_whitespace: false,
        abort_signal: None,
    }
}

fn grep_count(shared: &SharedFilePicker, query: &str) -> usize {
    let guard = shared.read().unwrap();
    let picker = guard.as_ref().unwrap();
    let parsed = parse_grep_query(query);
    picker.grep(&parsed, &grep_opts()).matches.len()
}

fn set_degraded(shared: &SharedFilePicker, degraded: bool) {
    let guard = shared.read().unwrap();
    guard
        .as_ref()
        .unwrap()
        .set_watch_coverage_degraded(degraded);
}

fn wait_for_bigram(shared_picker: &SharedFilePicker) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let ready = shared_picker
            .read()
            .ok()
            .map(|guard| {
                guard
                    .as_ref()
                    .is_some_and(|p| !p.is_scan_active() && p.bigram_index().is_some())
            })
            .unwrap_or(false);
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Timed out waiting for bigram build"
        );
    }
}

fn stop(shared: &SharedFilePicker) {
    if let Ok(mut guard) = shared.write()
        && let Some(ref mut picker) = *guard
    {
        picker.stop_background_monitor();
    }
}

/// Core regression: a DROPPED watcher event on a DEGRADED root must NOT serve
/// stale content. The grow-in-place edit appends NEW_IDENT; with the degraded
/// flag set, the read path re-stats and serves fresh bytes.
#[test]
fn dropped_modify_event_refreshes_on_degraded_root() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    seed_corpus(base, 200);
    let target = base.join("config.rs");
    fs::write(&target, target_initial()).unwrap();

    let (shared, _frec) = boot(base);

    assert!(grep_count(&shared, OLD_IDENT) > 0, "old ident should match");
    assert_eq!(
        grep_count(&shared, NEW_IDENT),
        0,
        "new ident not yet present"
    );

    std::thread::sleep(Duration::from_millis(1100));
    // Edit on disk WITHOUT calling the handler — a missed watcher event.
    fs::write(&target, target_modified()).unwrap();

    // Mark coverage degraded (simulating the aborted-watch signal) and grep.
    set_degraded(&shared, true);

    assert!(
        grep_count(&shared, NEW_IDENT) > 0,
        "degraded root must re-stat and find the appended identifier"
    );
    assert!(
        grep_count(&shared, OLD_IDENT) > 0,
        "old ident still matches"
    );

    stop(&shared);
}

/// Half-fix detector: a SAME-SIZE in-place edit (no length change) must still
/// be caught on a degraded root. The recheck stats `(mtime, size)`; a
/// length-only check would skip the refresh for a same-size edit. The target's
/// new identifier is UNIQUE (absent from the corpus) so its match count
/// reflects only the target file.
#[test]
fn same_size_inplace_edit_refreshes_on_degraded_root() {
    // A 12-byte identifier replacing the 12-byte `RootRegistry`, unique to the
    // target file so corpus matches don't mask the result.
    const SAME_SIZE_NEW: &str = "McpConfigZzz";
    assert_eq!(OLD_IDENT.len(), SAME_SIZE_NEW.len(), "must be byte-equal");

    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    seed_corpus(base, 200);
    let target = base.join("config.rs");
    let initial = target_initial();
    fs::write(&target, &initial).unwrap();

    let (shared, _frec) = boot(base);

    assert_eq!(grep_count(&shared, SAME_SIZE_NEW), 0, "new ident absent");

    std::thread::sleep(Duration::from_millis(1100));
    // Replace every OLD_IDENT in the TARGET with the same-length identifier:
    // file size is unchanged, only mtime advances and bytes differ in place.
    let edited = initial.replace(OLD_IDENT, SAME_SIZE_NEW);
    assert_eq!(
        edited.len(),
        initial.len(),
        "edit must preserve byte length"
    );
    fs::write(&target, &edited).unwrap();

    set_degraded(&shared, true);

    let n = grep_count(&shared, SAME_SIZE_NEW);
    stop(&shared);
    assert!(
        n > 0,
        "degraded root must catch a same-size in-place edit via the (mtime, size) \
         recheck (a length-only check would miss it)"
    );
}

/// Gating proof: with the flag CLEAR the dropped edit stays stale (the healthy
/// path is intentionally zero-syscall and does NOT self-heal); flipping the
/// flag to set makes the SAME fixture refresh. Same picker, same edit.
#[test]
fn healthy_root_stays_fast_path_degraded_root_refreshes() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    seed_corpus(base, 200);
    let target = base.join("config.rs");
    fs::write(&target, target_initial()).unwrap();

    let (shared, _frec) = boot(base);
    // Warm the cache at the original size (see self_clearing test rationale).
    assert!(grep_count(&shared, OLD_IDENT) > 0, "warms the cache");
    assert_eq!(
        grep_count(&shared, NEW_IDENT),
        0,
        "new ident not yet present"
    );

    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&target, target_modified()).unwrap();

    // Flag clear (healthy): zero-syscall fast path serves the stale cache.
    assert_eq!(
        grep_count(&shared, NEW_IDENT),
        0,
        "healthy root must NOT re-stat: stays on the cached (stale) fast path"
    );

    // Flag set (degraded): the same fixture now refreshes.
    set_degraded(&shared, true);
    assert!(
        grep_count(&shared, NEW_IDENT) > 0,
        "degraded root re-stats and finds the new identifier"
    );

    stop(&shared);
}

/// Self-clearing semantics at the read layer: once the flag is cleared (the
/// watcher proved a clean re-watch), a subsequent grep takes the fast path
/// again — even with a further dropped edit it serves the cache.
#[test]
fn self_clearing_flag_returns_to_fast_path() {
    let tmp = TempDir::new().unwrap();
    let base = tmp.path();
    seed_corpus(base, 200);
    let target = base.join("config.rs");
    fs::write(&target, target_initial()).unwrap();

    let (shared, _frec) = boot(base);

    // Warm the persistent cache at the ORIGINAL size so the fast path is
    // deterministically stale for appended content (mmap maps the original
    // region; later appends fall outside it).
    assert!(grep_count(&shared, OLD_IDENT) > 0, "warms the cache");

    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&target, target_modified()).unwrap();

    // Degraded: refresh works.
    set_degraded(&shared, true);
    assert!(
        grep_count(&shared, NEW_IDENT) > 0,
        "degraded root finds appended identifier"
    );

    // A clean re-watch cleared the flag (simulated). Apply a NEW dropped edit;
    // the now-healthy root must take the fast path and NOT see it.
    set_degraded(&shared, false);
    std::thread::sleep(Duration::from_millis(1100));
    let second = format!("{}pub struct SecondNewType {{}}\n", target_modified());
    fs::write(&target, &second).unwrap();

    assert_eq!(
        grep_count(&shared, "SecondNewType"),
        0,
        "after self-clearing, the fast path is restored: no re-stat, stale cache served"
    );

    stop(&shared);
}
