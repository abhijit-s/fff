use std::time::Duration;

use fff_ipc::types::{FindOptions, GrepOptions, SearchResponse, WireGrepMode};

use crate::state::EngineState;

// Cold-start grep readiness gate. The initial scan runs on a background thread
// after the engine accepts connections, so the file list and content (bigram)
// index can both be incomplete when the first grep lands — yielding empty or
// partial results that look authoritative. Block grep until the picker reports
// the initial index ready, bounded so a pathologically slow scan never hangs a
// query indefinitely (callers then get correct-but-possibly-partial results
// rather than a stall).
const GREP_READINESS_TIMEOUT: Duration = Duration::from_secs(30);
const GREP_READINESS_POLL: Duration = Duration::from_millis(20);

// Poll the picker's readiness flag without holding the read lock across the
// sleep: take the lock briefly each tick, then drop it before awaiting.
async fn await_index_ready(state: &EngineState) {
    let picker_arc = state.shared_picker.clone();
    let deadline = tokio::time::Instant::now() + GREP_READINESS_TIMEOUT;
    loop {
        let ready = match picker_arc.read() {
            Ok(guard) => guard.as_ref().is_none_or(|p| p.is_index_ready()),
            Err(_) => true,
        };
        if ready || tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(GREP_READINESS_POLL).await;
    }
}

pub async fn handle_grep(
    state: &EngineState,
    query: String,
    options: GrepOptions,
) -> SearchResponse {
    use fff::{AiGrepConfig, QueryParser};
    use fff_ipc::types::WireGrepResponse;

    await_index_ready(state).await;

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let grep_options = to_core_grep_options(&options);

        let parser = QueryParser::new(AiGrepConfig);
        let parsed = parser.parse(&query);
        let result = picker.grep(&parsed, &grep_options);
        let wire_matches = project_grep_result(&result, picker);

        Ok::<_, String>(WireGrepResponse {
            matches: wire_matches,
            total_files_searched: result.total_files_searched,
            total_files: result.total_files,
            files_with_matches: result.files_with_matches,
            next_file_offset: result.next_file_offset,
            regex_fallback_error: result.regex_fallback_error,
        })
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::GrepResults(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_find_files(
    state: &EngineState,
    query: String,
    options: FindOptions,
) -> SearchResponse {
    use fff::{FuzzySearchOptions, PaginationArgs, QueryParser};
    use fff_ipc::types::WireSearchResult;

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let parser = QueryParser::default();
        let parsed = parser.parse(&query);

        let current_file_str = options.current_file.clone();
        let search_result = picker.fuzzy_search(
            &parsed,
            None,
            FuzzySearchOptions {
                max_threads: options.max_threads,
                current_file: current_file_str.as_deref(),
                project_path: None,
                combo_boost_score_multiplier: options.combo_boost_score_multiplier,
                min_combo_count: options.min_combo_count,
                pagination: PaginationArgs {
                    offset: options.offset,
                    limit: options.limit,
                },
            },
        );

        let wire: Vec<WireSearchResult> = search_result
            .items
            .iter()
            .zip(search_result.scores.iter())
            .map(|(item, score)| WireSearchResult {
                path: item.relative_path(picker),
                score: score.total,
                git_status: item.git_status.map(|s| s.bits()),
                frecency_score: item.total_frecency_score(),
            })
            .collect();

        Ok::<_, String>(wire)
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::SearchResults(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_multi_grep(
    state: &EngineState,
    patterns: Vec<String>,
    constraints: Option<String>,
    options: GrepOptions,
) -> SearchResponse {
    use fff::{AiGrepConfig, QueryParser};
    use fff_ipc::types::WireGrepResponse;

    await_index_ready(state).await;

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let grep_options = to_core_grep_options(&options);

        let parser = QueryParser::new(AiGrepConfig);
        let constraint_query = constraints.as_deref().unwrap_or("");
        let parsed_constraints = parser.parse(constraint_query);

        let patterns_refs: Vec<&str> = patterns.iter().map(|s| s.as_str()).collect();
        let result = picker.multi_grep(
            &patterns_refs,
            parsed_constraints.constraints.as_slice(),
            &grep_options,
        );

        let wire_matches = project_grep_result(&result, picker);

        Ok::<_, String>(WireGrepResponse {
            matches: wire_matches,
            total_files_searched: result.total_files_searched,
            total_files: result.total_files,
            files_with_matches: result.files_with_matches,
            next_file_offset: result.next_file_offset,
            regex_fallback_error: result.regex_fallback_error,
        })
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::GrepResults(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_list_recent_files(
    state: &EngineState,
    limit: usize,
    dirty_only: bool,
) -> SearchResponse {
    use fff_ipc::types::WireSearchResult;

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let mut items: Vec<_> = picker
            .get_files()
            .iter()
            .filter(|f| {
                !f.is_deleted()
                    && f.total_frecency_score() > 0
                    && (!dirty_only || f.git_status.is_some_and(fff::git::is_modified_status))
            })
            .map(|f| (f, f.total_frecency_score()))
            .collect();

        items.sort_unstable_by(|(_, a), (_, b)| b.cmp(a));
        items.truncate(limit);

        let wire: Vec<WireSearchResult> = items
            .into_iter()
            .map(|(f, score)| WireSearchResult {
                path: f.relative_path(picker),
                score,
                git_status: f.git_status.map(|s| s.bits()),
                frecency_score: score,
            })
            .collect();

        Ok::<_, String>(wire)
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::RecentFiles(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_get_git_status(state: &EngineState, include_clean: bool) -> SearchResponse {
    use fff::git::format_git_status_opt;
    use fff_ipc::types::WireGitFile;

    // The served cache is maintained only by FS-watcher events; macOS FSEvents
    // can coalesce/drop out-of-band edits (e.g. an editor saving a tracked file),
    // leaving it stale-clean. get_git_status is an explicit, human-paced query,
    // so recompute when the cache is older than the TTL — making the query
    // authoritative while rapid repeated calls reuse a fresh-enough result.
    const GIT_STATUS_TTL: Duration = Duration::from_secs(3);
    let needs_refresh = match state.last_git_refresh.lock() {
        Ok(g) => (*g).map_or(true, |t| t.elapsed() >= GIT_STATUS_TTL),
        Err(_) => true,
    };
    if needs_refresh {
        let picker = state.shared_picker.clone();
        let frecency = state.shared_frecency.clone();
        let _ = tokio::task::spawn_blocking(move || picker.refresh_git_status(&frecency)).await;
        if let Ok(mut g) = state.last_git_refresh.lock() {
            *g = Some(std::time::Instant::now());
        }
    }

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let wire: Vec<WireGitFile> = picker
            .get_files()
            .iter()
            .filter(|f| !f.is_deleted() && (f.git_status.is_some() || include_clean))
            .filter_map(|f| {
                let status_str = format_git_status_opt(f.git_status)?;
                if !include_clean && status_str == "clean" {
                    return None;
                }
                Some(WireGitFile {
                    path: f.relative_path(picker),
                    status: status_str.to_string(),
                    frecency_score: f.total_frecency_score(),
                })
            })
            .collect();

        Ok::<_, String>(wire)
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::GitStatus(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_list_directories(state: &EngineState, limit: usize) -> SearchResponse {
    use fff_ipc::types::WireDirEntry;

    let picker_arc = state.shared_picker.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let mut dirs: Vec<(&fff::types::DirItem, i32)> = picker
            .get_dirs()
            .iter()
            .filter(|d| d.relative_path_len() > 0)
            .map(|d| (d, d.max_access_frecency()))
            .collect();

        dirs.sort_unstable_by(|(_, a), (_, b)| b.cmp(a));
        dirs.truncate(limit);

        let wire: Vec<WireDirEntry> = dirs
            .into_iter()
            .map(|(d, max_frecency)| WireDirEntry {
                path: d.relative_path(picker),
                max_frecency,
            })
            .collect();

        Ok::<_, String>(wire)
    })
    .await;

    match result {
        Ok(Ok(wire)) => SearchResponse::Directories(wire),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

pub async fn handle_health(state: &EngineState) -> SearchResponse {
    use fff_ipc::types::{HealthResponse, RootHealth};

    let picker_arc = state.shared_picker.clone();
    let base_path = state.base_path.clone();
    let result = tokio::task::spawn_blocking(move || {
        let guard = picker_arc.read().map_err(|e| e.to_string())?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| "File picker not yet initialized".to_string())?;

        let indexed = picker.live_file_count() as u64;
        let dirty = picker
            .get_files()
            .iter()
            .filter(|f| !f.is_deleted() && f.git_status.is_some_and(fff::git::is_modified_status))
            .count() as u64;

        Ok::<_, String>(HealthResponse {
            roots: vec![RootHealth {
                slug: fff_ipc::base_path_slug(&base_path),
                base_path: base_path.to_string_lossy().into_owned(),
                indexed_files: Some(indexed),
                // Singleton has no per-root load timestamp — workers report this.
                last_scan_age_sec: None,
                watcher_backlog: None,
                dirty_count: Some(dirty),
            }],
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => SearchResponse::Health(resp),
        Ok(Err(msg)) => SearchResponse::Error(msg),
        Err(e) => SearchResponse::Error(format!("spawn_blocking join error: {e}")),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn to_core_grep_options(options: &GrepOptions) -> fff::grep::GrepSearchOptions {
    fff::grep::GrepSearchOptions {
        max_file_size: options.max_file_size,
        max_matches_per_file: options.max_matches_per_file,
        smart_case: options.smart_case,
        file_offset: options.file_offset,
        page_limit: options.page_limit,
        mode: wire_mode_to_grep_mode(options.mode),
        time_budget_ms: options.time_budget_ms,
        before_context: options.before_context,
        after_context: options.after_context,
        classify_definitions: options.classify_definitions,
        trim_whitespace: options.trim_whitespace,
        abort_signal: None,
    }
}

/// Project a `GrepResult` into owned wire types while the picker read-guard is held.
///
/// `FileItem.path` is a `ChunkedString` (arena-relative pointer) that becomes
/// invalid once the guard drops, so this must be called inside `spawn_blocking`
/// with the picker still borrowed.
fn project_grep_result(
    result: &fff::grep::GrepResult<'_>,
    picker: &fff::file_picker::FilePicker,
) -> Vec<fff_ipc::types::WireGrepFileMatches> {
    use fff_ipc::types::{WireGrepFileMatches, WireGrepMatch};
    use std::collections::HashMap;

    let mut by_file: HashMap<usize, WireGrepFileMatches> = HashMap::new();
    for m in &result.matches {
        let file = result.files[m.file_index];
        let entry = by_file
            .entry(m.file_index)
            .or_insert_with(|| WireGrepFileMatches {
                path: file.relative_path(picker),
                size: file.size,
                git_status: file.git_status.map(|s| s.bits()),
                frecency_score: file.total_frecency_score(),
                matches: Vec::new(),
            });
        entry.matches.push(WireGrepMatch {
            line_number: m.line_number,
            col: m.col,
            line_text: m.line_content.clone(),
            match_byte_offsets: m.match_byte_offsets.iter().copied().collect(),
            is_definition: m.is_definition,
            context_before: m.context_before.clone(),
            context_after: m.context_after.clone(),
        });
    }
    // Preserve file ordering from result.files.
    let mut ordered: Vec<WireGrepFileMatches> = Vec::new();
    for i in 0..result.files.len() {
        if let Some(fm) = by_file.remove(&i) {
            ordered.push(fm);
        }
    }
    ordered
}

fn wire_mode_to_grep_mode(mode: WireGrepMode) -> fff::grep::GrepMode {
    match mode {
        WireGrepMode::PlainText => fff::grep::GrepMode::PlainText,
        WireGrepMode::Regex => fff::grep::GrepMode::Regex,
        WireGrepMode::Fuzzy => fff::grep::GrepMode::Fuzzy,
    }
}
