use std::path::Path;

pub(crate) const NON_GIT_IGNORED_DIRS: &[&str] = &[
    "node_modules",
    "__pycache__",
    "venv",
    ".venv",
    // Rust (these are glob-only patterns for non_git_repo_overrides,
    // is_non_code_directory matches the "target" component separately)
    "target/debug",
    "target/release",
    "target/rust-analyzer",
    "target/criterion",
];

#[cfg(target_os = "macos")]
pub(crate) const PLATFORM_IGNORED_DIRS: &[&str] = &[
    "Library/Application Support",
    "Library/Caches",
    // App-group sandbox storage — used by iMessage, Photos, Notes, Calendar,
    // Electron apps, etc. for SQLite-WAL, LevelDB, protobuf files. These are
    // almost entirely extension-less binary files (~80k on a typical $HOME)
    // that never need to appear in a fuzzy or grep search.
    "Library/Group Containers",
    "Library/Containers",
];

#[cfg(target_os = "windows")]
pub(crate) const PLATFORM_IGNORED_DIRS: &[&str] = &[
    "bin/Debug",
    "bin/Release",
    "Program Files",
    "Program Files (x86)",
    "AppData/Local",
    "AppData/Roaming",
];

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) const PLATFORM_IGNORED_DIRS: &[&str] = &[];

pub(crate) fn non_git_repo_overrides(base_path: &Path) -> Option<ignore::overrides::Override> {
    use ignore::overrides::OverrideBuilder;

    let mut builder = OverrideBuilder::new(base_path);
    for dir in NON_GIT_IGNORED_DIRS.iter().chain(PLATFORM_IGNORED_DIRS) {
        let pattern = format!("!**/{dir}/");
        if let Err(e) = builder.add(&pattern) {
            tracing::warn!("failed to add ignore pattern {pattern}: {e}");
        }
    }

    builder.build().ok()
}

/// Build a gitignore matcher from user-supplied patterns rooted at `base_path`.
/// Returns None when there are no patterns (callers skip filtering). Patterns
/// use standard gitignore syntax (bare glob excludes, leading `!` re-includes).
pub(crate) fn user_ignore_matcher(
    base_path: &Path,
    patterns: &[String],
) -> Option<ignore::gitignore::Gitignore> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new(base_path);
    for p in patterns {
        if let Err(e) = builder.add_line(None, p) {
            tracing::warn!("invalid ignore pattern {p:?}: {e}");
        }
    }
    match builder.build() {
        Ok(gi) => Some(gi),
        Err(e) => {
            tracing::warn!("failed to build ignore matcher: {e}");
            None
        }
    }
}

pub(crate) fn is_non_code_directory(path: &Path) -> bool {
    let path_str = path.as_os_str().to_str().unwrap_or("");
    NON_GIT_IGNORED_DIRS
        .iter()
        .chain(PLATFORM_IGNORED_DIRS)
        .any(|&dir| {
            #[cfg(target_os = "windows")]
            let dir = dir.replace('/', std::path::MAIN_SEPARATOR_STR);
            #[cfg(target_os = "windows")]
            return path_str.contains(dir.as_str());

            #[cfg(not(target_os = "windows"))]
            path_str.contains(dir)
        })
}
