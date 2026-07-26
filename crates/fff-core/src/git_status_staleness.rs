//! Regression test for stale git status after an out-of-band edit.
//!
//! `get_git_status` serves each `FileItem.git_status`, a cache maintained only by
//! FS-watcher events. A dropped/coalesced macOS FSEvent (e.g. an editor saving a
//! tracked file) left it stale-clean. The fix makes the serve path recompute via
//! `SharedFilePicker::refresh_git_status`. This drives `FilePicker` with
//! `watch: false` so NO event is ever delivered, then asserts a refresh surfaces
//! the edit — the mechanism the engine's `handle_get_git_status` now invokes.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use crate::file_picker::{FFFMode, FilePicker};
use crate::{FilePickerOptions, SharedFilePicker, SharedFrecency};

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .status()
        .expect("spawn git")
        .success();
    assert!(ok, "git {args:?} failed");
}

fn boot(base: &Path) -> (SharedFilePicker, SharedFrecency) {
    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();

    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        FilePickerOptions {
            base_path: base.to_string_lossy().to_string(),
            mode: FFFMode::Neovim,
            watch: false,
            ..Default::default()
        },
    )
    .expect("Failed to create FilePicker");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        std::thread::sleep(Duration::from_millis(50));
        let ready = shared_picker
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|p| !p.is_scan_active()))
            .unwrap_or(false);
        if ready {
            break;
        }
        assert!(Instant::now() < deadline, "Timed out waiting for scan");
    }
    (shared_picker, shared_frecency)
}

/// Live-recompute git status, then read the target file's cached status.
fn status_after_refresh(
    shared: &SharedFilePicker,
    frecency: &SharedFrecency,
    rel: &str,
) -> Option<git2::Status> {
    shared
        .refresh_git_status(frecency)
        .expect("refresh git status");
    let guard = shared.read().unwrap();
    let picker = guard.as_ref().unwrap();
    picker
        .get_files()
        .iter()
        .find(|f| f.relative_path(picker) == rel)
        .and_then(|f| f.git_status)
}

fn stop(shared: &SharedFilePicker) {
    if let Ok(mut guard) = shared.write()
        && let Some(ref mut picker) = *guard
    {
        picker.stop_background_monitor();
    }
}

/// A tracked file edited out-of-band (no watcher event, since `watch: false`)
/// must show as modified once git status is recomputed on the serve path.
#[test]
fn refresh_surfaces_out_of_band_edit_without_watcher_event() {
    let tmp = TempDir::new().unwrap();
    // Canonicalize so the picker's base_path matches libgit2's canonicalized
    // workdir (macOS temp dirs live under the /var -> /private/var symlink);
    // git-status keys are absolute, so a mismatch would never map onto FileItems.
    let base = &fs::canonicalize(tmp.path()).unwrap();

    git(base, &["init", "-q"]);
    fs::write(base.join("tracked.rs"), "fn main() {}\n").unwrap();
    git(base, &["add", "."]);
    git(base, &["commit", "-q", "-m", "init"]);

    let (picker, frecency) = boot(base);

    // Committed + clean → no working-tree modification.
    let before = status_after_refresh(&picker, &frecency, "tracked.rs");
    assert!(
        before.map_or(true, |s| !s.is_wt_modified()),
        "expected clean before edit, got {before:?}"
    );

    // Out-of-band edit — nothing delivers a watcher event.
    fs::write(base.join("tracked.rs"), "fn main() { let x = 1; }\n").unwrap();

    // What handle_get_git_status now does on the serve path: recompute.
    let after = status_after_refresh(&picker, &frecency, "tracked.rs");
    assert!(
        after.is_some_and(|s| s.is_wt_modified()),
        "git status must reflect the out-of-band edit after refresh, got {after:?}"
    );

    stop(&picker);
}
