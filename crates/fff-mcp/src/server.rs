use crate::cursor::CursorStore;
use crate::output::{GrepFormatter, OutputMode, file_suffix};
use fff::grep::{GrepMode, GrepSearchOptions, has_regex_metacharacters};
use fff::types::{FileItem, PaginationArgs};
use fff::{FuzzySearchOptions, QueryParser, SharedFilePicker, SharedFrecency};
use fff_query_parser::AiGrepConfig;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const SCAN_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn normalize_max_results(raw: Option<f64>, default: usize) -> usize {
    match raw {
        None => default,
        Some(v) if v <= 0.0 || !v.is_finite() => default,
        Some(v) => (v.round() as usize).max(1),
    }
}

fn cleanup_fuzzy_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if !matches!(c, ':' | '-' | '_') {
            out.extend(c.to_lowercase());
        }
    }
    out
}

fn make_grep_options(
    output_mode: OutputMode,
    mode: GrepMode,
    file_offset: usize,
    context: Option<usize>,
) -> (GrepSearchOptions, bool) {
    let is_usage = output_mode == OutputMode::Usage;
    let matches_per_file = match output_mode {
        OutputMode::FilesWithMatches => 1,
        _ if is_usage => 8,
        _ => 10,
    };
    let ctx_lines = if is_usage {
        context.unwrap_or(1)
    } else {
        context.unwrap_or(0)
    };
    let auto_expand = !is_usage && ctx_lines == 0;
    let after_ctx = if auto_expand { 8 } else { ctx_lines };

    (
        GrepSearchOptions {
            max_file_size: 10 * 1024 * 1024,
            max_matches_per_file: matches_per_file,
            smart_case: true,
            file_offset,
            page_limit: 50,
            mode,
            time_budget_ms: 0,
            before_context: ctx_lines,
            after_context: after_ctx,
            classify_definitions: true,
            trim_whitespace: true,
            abort_signal: None,
        },
        auto_expand,
    )
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct FindFilesParams {
    /// Fuzzy search query. Supports path prefixes and glob constraints.
    // `pattern` alias for consistency with grep's alias and the common
    // file-search parameter name (#311).
    #[serde(alias = "pattern")]
    pub query: String,
    /// Max results (default 20).
    #[serde(rename = "maxResults")]
    // this has to be float because llms are stupid
    pub max_results: Option<f64>,
    /// Cursor from previous result. Only use if previous results weren't sufficient.
    pub cursor: Option<String>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    /// Call `list_roots` to discover available roots.
    #[serde(default)]
    pub base_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GrepParams {
    /// Search text or regex query with optional constraint prefixes.
    /// Matches within single lines only — use ONE specific term, not multiple words.
    // `pattern` alias: LLMs that have seen multi_grep (which uses `patterns`)
    // routinely call grep with `pattern`; accept it instead of erroring out
    // with an unhelpful "missing field `query`" (#311).
    #[serde(alias = "pattern")]
    pub query: String,
    /// Max matching lines (default 20).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>, // this has to be float because llms are stupid
    /// Cursor from previous result. Only use if previous results weren't sufficient.
    pub cursor: Option<String>,
    /// Output format (default 'content').
    pub output_mode: Option<String>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    /// Call `list_roots` to discover available roots.
    #[serde(default)]
    pub base_path: Option<String>,
}

fn deserialize_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct PatternsVisitor;

    impl<'de> de::Visitor<'de> for PatternsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, an array of strings, or a stringified JSON array")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            // Try to parse as JSON array first
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(v)
            {
                return Ok(parsed);
            }
            Ok(vec![v.to_string()])
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            if v.starts_with('[')
                && let Ok(parsed) = serde_json::from_str::<Vec<String>>(&v)
            {
                return Ok(parsed);
            }
            Ok(vec![v])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(PatternsVisitor)
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct MultiGrepParams {
    /// Patterns to match (OR logic). Include all naming conventions: snake_case, PascalCase, camelCase.
    #[serde(deserialize_with = "deserialize_patterns")]
    pub patterns: Vec<String>,
    /// File constraints (e.g. '*.{ts,tsx} !test/'). ALWAYS provide when possible.
    pub constraints: Option<String>,
    /// Max matching lines (default 20).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>,
    /// Cursor from previous result.
    pub cursor: Option<String>,
    /// Output format (default 'content').
    pub output_mode: Option<String>,
    /// Context lines before/after each match.
    pub context: Option<f64>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    /// Call `list_roots` to discover available roots.
    #[serde(default)]
    pub base_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RecordAccessParams {
    /// File path to record as accessed. Relative to project root or absolute.
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListRecentFilesParams {
    /// Maximum results (default 20).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>,
    /// When true, only include files with uncommitted changes (default false).
    pub dirty_only: Option<bool>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    #[serde(default)]
    pub base_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GetGitStatusParams {
    /// When true, include clean files as well (default false — only dirty files).
    pub include_clean: Option<bool>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    #[serde(default)]
    pub base_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListDirectoriesParams {
    /// Maximum results (default 30).
    #[serde(rename = "maxResults")]
    pub max_results: Option<f64>,
    /// Optional. Target a specific registered root by absolute path. Omit to
    /// use the default root. Multi-root requires fff-engine in master mode.
    #[serde(default)]
    pub base_path: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListRootsParams {}

#[derive(Clone)]
pub struct FffServer {
    picker: SharedFilePicker,
    frecency: SharedFrecency,
    cursor_store: Arc<Mutex<CursorStore>>,
    update_notice_sent: Arc<AtomicBool>,
    /// Unix proxy path: per-root connection pool. When `Some`, all tool
    /// handlers route through fff-engine via the pool rather than calling
    /// fff-core directly.
    #[cfg(unix)]
    pool: Option<Arc<crate::pool::ConnectionPool>>,
    /// Startup-time registry of known roots (default + extras via `--root`).
    #[cfg(unix)]
    registry: Option<Arc<crate::registry::RootRegistry>>,
    last_activity: Arc<AtomicU64>,
    scan_ready: Arc<AtomicBool>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FffServer {
    pub fn new(picker: SharedFilePicker, frecency: SharedFrecency) -> Self {
        Self {
            picker,
            frecency,
            cursor_store: Arc::new(Mutex::new(CursorStore::new())),
            update_notice_sent: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            pool: None,
            #[cfg(unix)]
            registry: None,
            last_activity: Arc::new(AtomicU64::new(now_secs())),
            scan_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Create a proxy server backed by fff-engine (Unix only).
    #[cfg(unix)]
    pub fn new_proxy(
        pool: Arc<crate::pool::ConnectionPool>,
        registry: Arc<crate::registry::RootRegistry>,
    ) -> Self {
        use fff::{SharedFilePicker, SharedFrecency};
        Self {
            picker: SharedFilePicker::default(),
            frecency: SharedFrecency::default(),
            cursor_store: Arc::new(Mutex::new(CursorStore::new())),
            update_notice_sent: Arc::new(AtomicBool::new(false)),
            pool: Some(pool),
            registry: Some(registry),
            last_activity: Arc::new(AtomicU64::new(now_secs())),
            scan_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Resolve the effective base_path for a tool call. Returns
    /// `Ok(Some(path))` to dispatch via the pool, `Ok(None)` to fall through
    /// to direct mode, or `Err(...)` for the multi-root-without-master case.
    ///
    /// Accepts either a registered root **name** (matched first) or a path.
    #[cfg(unix)]
    fn resolve_route(
        &self,
        explicit: Option<&str>,
    ) -> Result<Option<std::path::PathBuf>, ErrorData> {
        let Some(registry) = self.registry.as_ref() else {
            return Ok(None);
        };
        match explicit {
            None => Ok(Some(registry.default_root().to_path_buf())),
            Some(raw) => {
                if let Some(by_name) = registry.resolve_name(raw) {
                    if registry.is_default(by_name) {
                        return Ok(Some(registry.default_root().to_path_buf()));
                    }
                    if !master_mode_available() {
                        return Err(ErrorData::invalid_params(
                            "multi-root requires master mode; start fff-engine in master mode \
                             or omit the base_path argument"
                                .to_string(),
                            None,
                        ));
                    }
                    return Ok(Some(by_name.to_path_buf()));
                }
                let path = std::path::PathBuf::from(raw);
                if !path.is_absolute() {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "base_path {raw:?} is neither a registered root name nor an absolute path; \
                             call list_roots to see available roots"
                        ),
                        None,
                    ));
                }
                if registry.is_default(&path) {
                    return Ok(Some(registry.default_root().to_path_buf()));
                }
                if !master_mode_available() {
                    return Err(ErrorData::invalid_params(
                        "multi-root requires master mode; start fff-engine in master mode \
                         or omit the base_path argument"
                            .to_string(),
                        None,
                    ));
                }
                Ok(Some(path))
            }
        }
    }

    /// Dispatch a SearchRequest via the connection pool. Returns None when
    /// the server is not in proxy mode (no pool configured).
    #[cfg(unix)]
    fn dispatch(
        &self,
        base_path: &std::path::Path,
        req: &fff_ipc::types::SearchRequest,
    ) -> Option<fff_ipc::types::SearchResponse> {
        let pool = self.pool.as_ref()?;
        Some(pool.search_with_retry(base_path, req))
    }

    pub fn last_activity(&self) -> Arc<AtomicU64> {
        self.last_activity.clone()
    }

    fn bump_activity(&self) {
        self.last_activity.store(now_secs(), Ordering::Relaxed);
    }

    fn wait_for_scan(&self, timeout: std::time::Duration) -> Result<(), ErrorData> {
        if self.scan_ready.load(Ordering::Relaxed) {
            return Ok(());
        }

        let deadline = std::time::Instant::now() + timeout;

        loop {
            let is_scanning = self
                .picker
                .read()
                .ok()
                .as_ref()
                .and_then(|g| g.as_ref())
                .map(|p| p.is_scan_active())
                .unwrap_or(true);

            if !is_scanning {
                self.scan_ready.store(true, Ordering::Relaxed);
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(ErrorData::internal_error(
                    "Index is still building; retry shortly",
                    None,
                ));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn lock_cursors(&self) -> Result<std::sync::MutexGuard<'_, CursorStore>, ErrorData> {
        self.cursor_store.lock().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire cursor store lock: {e}"), None)
        })
    }

    fn maybe_append_update_notice(&self, result: &mut CallToolResult) {
        if self.update_notice_sent.swap(true, Ordering::Relaxed) {
            return;
        }
        let notice = crate::update_check::get_update_notice();
        if notice.is_empty() {
            // Reset so the next call can try again (check may still be in flight)
            self.update_notice_sent.store(false, Ordering::Relaxed);
            return;
        }
        result.content.push(Content::text(notice));
    }

    fn perform_grep(
        &self,
        query: &str,
        mode: GrepMode,
        max_results: usize,
        cursor_id: Option<&str>,
        output_mode: OutputMode,
        context: Option<usize>,
    ) -> Result<CallToolResult, ErrorData> {
        let file_offset = cursor_id
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let (options, auto_expand) = make_grep_options(output_mode, mode, file_offset, context);
        let ctx_lines = options.before_context;

        // Acquire picker lock once for the entire operation.
        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;

        let parser = QueryParser::new(AiGrepConfig);
        let parsed = parser.parse(query);
        let result = picker.grep(&parsed, &options);

        if result.matches.is_empty() && file_offset == 0 {
            // Auto-retry: try broadening multi-word queries by dropping first non-constraint word
            let parts: Vec<&str> = query.split_whitespace().collect();
            if parts.len() >= 2 {
                let first_word = parts[0];
                let is_valid_constraint = first_word.starts_with('!')
                    || first_word.starts_with('*')
                    || first_word.ends_with('/');

                if !is_valid_constraint {
                    let rest_query = parts[1..].join(" ");
                    let rest_parsed = parser.parse(&rest_query);

                    let rest_text = rest_parsed.grep_text();
                    let retry_mode = if has_regex_metacharacters(&rest_text) {
                        GrepMode::Regex
                    } else {
                        mode
                    };

                    let (retry_options, _) = make_grep_options(output_mode, retry_mode, 0, context);
                    let retry_result = picker.grep(&rest_parsed, &retry_options);

                    if !retry_result.matches.is_empty() && retry_result.matches.len() <= 10 {
                        let mut cs = self.lock_cursors()?;
                        let text = &GrepFormatter {
                            matches: &retry_result.matches,
                            files: &retry_result.files,
                            total_matched: retry_result.matches.len(),
                            next_file_offset: retry_result.next_file_offset,
                            output_mode,
                            max_results,
                            show_context: ctx_lines > 0,
                            auto_expand_defs: auto_expand,
                            picker,
                        }
                        .format(&mut cs);
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "0 matches for '{}'. Auto-broadened to '{}':\n{}",
                            query, rest_query, text
                        ))]));
                    }
                }
            }

            // Fuzzy fallback for typo tolerance
            let fuzzy_query = cleanup_fuzzy_query(query);
            let (fuzzy_options, _) = make_grep_options(output_mode, GrepMode::Fuzzy, 0, Some(0));
            let fuzzy_parsed = parser.parse(&fuzzy_query);
            let fuzzy_result = picker.grep(&fuzzy_parsed, &fuzzy_options);

            if !fuzzy_result.matches.is_empty() {
                let mut lines: Vec<String> = Vec::new();
                lines.push(format!(
                    "0 exact matches. {} approximate:",
                    fuzzy_result.matches.len()
                ));
                let mut current_file = String::new();
                for m in fuzzy_result.matches.iter().take(3) {
                    let file = fuzzy_result.files[m.file_index];
                    let file_rel = file.relative_path(picker);
                    if file_rel != current_file {
                        current_file = file_rel;
                        lines.push(current_file.to_string());
                    }
                    lines.push(format!(" {}: {}", m.line_number, m.line_content));
                }
                return Ok(CallToolResult::success(vec![Content::text(
                    lines.join("\n"),
                )]));
            }

            // File path fallback: if query looks like a path, suggest the matching file
            if query.contains('/') {
                let file_parser = QueryParser::default();
                let file_query = file_parser.parse(query);
                let file_opts = FuzzySearchOptions {
                    max_threads: 0,
                    current_file: None,
                    project_path: Some(picker.base_path()),
                    combo_boost_score_multiplier: 100,
                    min_combo_count: 3,
                    pagination: PaginationArgs {
                        offset: 0,
                        limit: 1,
                    },
                };
                let file_result = picker.fuzzy_search(&file_query, None, file_opts);
                if let (Some(top), Some(score)) =
                    (file_result.items.first(), file_result.scores.first())
                {
                    // Only suggest when the match is strong enough.
                    let query_len = query.len() as i32;
                    if score.base_score > query_len * 10 {
                        return Ok(CallToolResult::success(vec![Content::text(format!(
                            "0 content matches. But there is a relevant file path: {}",
                            top.relative_path(picker)
                        ))]));
                    }
                }
            }

            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        if result.matches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        let mut cs = self.lock_cursors()?;
        let text = &GrepFormatter {
            matches: &result.matches,
            files: &result.files,
            total_matched: result.matches.len(),
            next_file_offset: result.next_file_offset,
            output_mode,
            max_results,
            show_context: ctx_lines > 0,
            auto_expand_defs: auto_expand,
            picker,
        }
        .format(&mut cs);

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    // ── Proxy dispatch (Unix only) ────────────────────────────────────────────

    #[cfg(unix)]
    fn proxy_grep(
        &self,
        query: &str,
        max_results: usize,
        cursor_id: Option<&str>,
        output_mode: OutputMode,
        context: Option<usize>,
        base_path_arg: Option<&str>,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use crate::output::wire::WireGrepFormatter;
        use fff::grep::has_regex_metacharacters;
        use fff::{AiGrepConfig, QueryParser};
        use fff_ipc::types::{GrepOptions, SearchRequest, SearchResponse, WireGrepMode};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(base_path_arg) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };

        let file_offset = cursor_id
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let (options_core, auto_expand) = make_grep_options(
            output_mode,
            {
                let parsed = QueryParser::new(AiGrepConfig).parse(query);
                if has_regex_metacharacters(&parsed.grep_text()) {
                    GrepMode::Regex
                } else {
                    GrepMode::PlainText
                }
            },
            file_offset,
            context,
        );

        let ctx_lines = options_core.before_context;
        let wire_mode = match options_core.mode {
            GrepMode::PlainText => WireGrepMode::PlainText,
            GrepMode::Regex => WireGrepMode::Regex,
            GrepMode::Fuzzy => WireGrepMode::Fuzzy,
        };

        let req = SearchRequest::Grep {
            query: query.to_owned(),
            options: GrepOptions {
                max_file_size: options_core.max_file_size,
                max_matches_per_file: options_core.max_matches_per_file,
                smart_case: options_core.smart_case,
                file_offset: options_core.file_offset,
                page_limit: options_core.page_limit,
                mode: wire_mode,
                time_budget_ms: options_core.time_budget_ms,
                before_context: options_core.before_context,
                after_context: options_core.after_context,
                classify_definitions: options_core.classify_definitions,
                trim_whitespace: options_core.trim_whitespace,
            },
        };

        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::GrepResults(wire) => {
                if wire.matches.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "0 matches.",
                    )])));
                }
                let mut cs = self.lock_cursors().ok()?;
                let text = WireGrepFormatter {
                    response: &wire,
                    output_mode,
                    max_results,
                    show_context: ctx_lines > 0,
                    auto_expand_defs: auto_expand,
                }
                .format(&mut cs);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn proxy_find_files(
        &self,
        query: &str,
        max_results: usize,
        cursor_id: Option<&str>,
        base_path_arg: Option<&str>,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use crate::output::wire::format_wire_find_files;
        use fff_ipc::types::{FindOptions, SearchRequest, SearchResponse};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(base_path_arg) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };

        let page_offset = cursor_id
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let req = SearchRequest::FindFiles {
            query: query.to_owned(),
            options: FindOptions {
                max_threads: 0,
                current_file: None,
                combo_boost_score_multiplier: 100,
                min_combo_count: 3,
                offset: page_offset,
                limit: max_results,
            },
        };

        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::SearchResults(items) => {
                if items.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "0 results",
                    )])));
                }
                let total_matched = items.len(); // wire doesn't carry total yet
                let mut cs = self.lock_cursors().ok()?;
                let text = format_wire_find_files(&items, total_matched, page_offset, &mut cs);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn proxy_record_access(&self, path: &str) -> Option<Result<CallToolResult, ErrorData>> {
        let pool = self.pool.as_ref()?;
        let registry = self.registry.as_ref()?;
        // Longest-prefix routing: pick the root that owns this file path.
        let base_path = registry.root_for_path(std::path::Path::new(path));
        // Fire-and-forget: the engine sends no reply for RecordAccess, so we
        // must not dispatch through the blocking request/response path.
        pool.record_access(&base_path, path);
        Some(Ok(CallToolResult::success(vec![Content::text("ok")])))
    }

    #[cfg(unix)]
    fn proxy_list_recent_files(
        &self,
        limit: usize,
        dirty_only: bool,
        base_path_arg: Option<&str>,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use fff_ipc::types::{SearchRequest, SearchResponse};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(base_path_arg) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        let req = SearchRequest::ListRecentFiles { limit, dirty_only };
        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::RecentFiles(items) => {
                if items.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "No recently accessed files.",
                    )])));
                }
                let text = format_recent_files_wire(&items);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn proxy_get_git_status(
        &self,
        include_clean: bool,
        base_path_arg: Option<&str>,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use fff_ipc::types::{SearchRequest, SearchResponse};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(base_path_arg) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        let req = SearchRequest::GetGitStatus { include_clean };
        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::GitStatus(items) => {
                if items.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "No uncommitted changes.",
                    )])));
                }
                let text = format_git_status_wire(&items);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn proxy_list_directories(
        &self,
        limit: usize,
        base_path_arg: Option<&str>,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use fff_ipc::types::{SearchRequest, SearchResponse};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(base_path_arg) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };
        let req = SearchRequest::ListDirectories { limit };
        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::Directories(items) => {
                if items.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "No directories found.",
                    )])));
                }
                let text = format_directories_wire(&items);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn proxy_multi_grep(
        &self,
        params: &MultiGrepParams,
        max_results: usize,
    ) -> Option<Result<CallToolResult, ErrorData>> {
        use crate::output::wire::WireGrepFormatter;
        use fff_ipc::types::{GrepOptions, SearchRequest, SearchResponse, WireGrepMode};

        self.pool.as_ref()?;
        let base_path = match self.resolve_route(params.base_path.as_deref()) {
            Ok(Some(p)) => p,
            Ok(None) => return None,
            Err(e) => return Some(Err(e)),
        };

        let file_offset = params
            .cursor
            .as_deref()
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let context = params.context.map(|v| v.round() as usize);
        let output_mode = OutputMode::new(params.output_mode.as_deref());
        let (options_core, auto_expand) =
            make_grep_options(output_mode, GrepMode::PlainText, file_offset, context);
        let ctx_lines = options_core.before_context;

        let req = SearchRequest::MultiGrep {
            patterns: params.patterns.clone(),
            constraints: params.constraints.clone(),
            options: GrepOptions {
                max_file_size: options_core.max_file_size,
                max_matches_per_file: options_core.max_matches_per_file,
                smart_case: options_core.smart_case,
                file_offset: options_core.file_offset,
                page_limit: options_core.page_limit,
                mode: WireGrepMode::PlainText,
                time_budget_ms: options_core.time_budget_ms,
                before_context: options_core.before_context,
                after_context: options_core.after_context,
                classify_definitions: options_core.classify_definitions,
                trim_whitespace: options_core.trim_whitespace,
            },
        };

        let response = self.dispatch(&base_path, &req)?;

        match response {
            SearchResponse::GrepResults(wire) => {
                if wire.matches.is_empty() {
                    return Some(Ok(CallToolResult::success(vec![Content::text(
                        "0 matches.",
                    )])));
                }
                let mut cs = self.lock_cursors().ok()?;
                let text = WireGrepFormatter {
                    response: &wire,
                    output_mode,
                    max_results,
                    show_context: ctx_lines > 0,
                    auto_expand_defs: auto_expand,
                }
                .format(&mut cs);
                Some(Ok(CallToolResult::success(vec![Content::text(text)])))
            }
            SearchResponse::Error(msg) => Some(Err(ErrorData::internal_error(
                format!("fff-engine error: {msg}"),
                None,
            ))),
            _ => None,
        }
    }
}

#[tool_router]
impl FffServer {
    /// Fuzzy file search by name. Searches FILE NAMES, not file contents.
    /// Use it when you need to find a file, not a definition.
    /// Use grep instead for searching code content (definitions, usage patterns).
    /// Supports fuzzy matching, path prefixes ('shc/'), and glob constraints.
    /// IMPORTANT: Keep queries SHORT — prefer 1-2 terms max.
    #[tool(
        name = "find_files",
        description = "Fuzzy file search by name. Searches FILE NAMES, not file contents. Use it when you need to find a file, not a definition. Use grep instead for searching code content (definitions, usage patterns). Supports fuzzy matching, path prefixes ('src/'), and glob constraints ('name **/src/*.{ts,tsx} !test/'). IMPORTANT: Keep queries SHORT — prefer 1-2 terms max. Multiple words are a waterfall (each narrows results), NOT OR. If unsure, start broad with 1 term and refine.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn find_files(
        &self,
        Parameters(params): Parameters<FindFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.bump_activity();

        let max_results = normalize_max_results(params.max_results, 20);
        let query = &params.query;

        #[cfg(unix)]
        if let Some(result) = self.proxy_find_files(
            query,
            max_results,
            params.cursor.as_deref(),
            params.base_path.as_deref(),
        ) {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        // Local (non-proxy) path only: the scan-readiness gate polls the
        // in-process picker, which is absent in proxy mode.
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let page_offset = params
            .cursor
            .as_deref()
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;
        let base_path = picker.base_path();
        let make_opts = |offset: usize| FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(base_path),
            combo_boost_score_multiplier: 100,
            min_combo_count: 3,
            pagination: PaginationArgs {
                offset,
                limit: max_results,
            },
        };

        let parser = QueryParser::default();
        let fff_query = parser.parse(query);
        let result = picker.fuzzy_search(&fff_query, None, make_opts(page_offset));
        let total_files = result.total_files;

        // Auto-retry with fewer terms if 3+ words return 0 results
        let words: Vec<&str> = query.split_whitespace().collect();
        let shorter = words.get(..2).map(|w| w.join(" "));

        let (items, scores, total_matched) =
            if result.items.is_empty() && words.len() >= 3 && page_offset == 0 {
                if let Some(shorter) = &shorter {
                    let shorter_query = parser.parse(shorter);
                    let retry = picker.fuzzy_search(&shorter_query, None, make_opts(0));

                    (retry.items, retry.scores, retry.total_matched)
                } else {
                    (result.items, result.scores, result.total_matched)
                }
            } else {
                (result.items, result.scores, result.total_matched)
            };

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "0 results ({} indexed)",
                total_files
            ))]));
        }

        let mut lines: Vec<String> = Vec::new();
        let top_item = items[0];
        let is_exact_match = scores[0].exact_match;

        if page_offset == 0 {
            if is_exact_match {
                lines.push(format!(
                    "→ Read {} (exact match!)",
                    top_item.relative_path(picker)
                ));
            } else if scores.len() < 2 || scores[0].total > scores[1].total.saturating_mul(2) {
                lines.push(format!(
                    "→ Read {} (best match — Read this file directly)",
                    top_item.relative_path(picker)
                ));
            }
        }

        let next_offset = page_offset + items.len();
        let has_more = next_offset < total_matched;

        if has_more {
            lines.push(format!("{}/{} matches", items.len(), total_matched));
        }

        for item in &items {
            lines.push(format!(
                "{}{}",
                item.relative_path(picker),
                file_suffix(item.git_status, item.total_frecency_score())
            ));
        }

        if has_more {
            let mut cs = self.lock_cursors()?;
            let cursor_id = cs.store(next_offset);
            lines.push(format!("cursor: {}", cursor_id));
        }

        let mut result = CallToolResult::success(vec![Content::text(lines.join("\n"))]);
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// Search file contents for text patterns. This is the DEFAULT search tool.
    /// Prefer plain text over regex. Filter files with constraints.
    #[tool(
        name = "grep",
        description = "Search file contents. Search for bare identifiers (e.g. 'InProgressQuote', 'ActorAuth'), NOT code syntax or regex. Filter files with constraints (e.g. '*.rs query', 'src/ query'). Use filename, directory (ending with /) or glob expressions to prefilter. See server instructions for constraint syntax and core rules.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn grep(
        &self,
        Parameters(params): Parameters<GrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        self.bump_activity();

        let max_results = normalize_max_results(params.max_results, 20);
        let output_mode = OutputMode::new(params.output_mode.as_deref());

        #[cfg(unix)]
        if let Some(result) = self.proxy_grep(
            &params.query,
            max_results,
            params.cursor.as_deref(),
            output_mode,
            None,
            params.base_path.as_deref(),
        ) {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        // Local (non-proxy) path only: the scan-readiness gate polls the
        // in-process picker, which is absent in proxy mode.
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let parsed = QueryParser::new(AiGrepConfig).parse(&params.query);
        let grep_text = parsed.grep_text();

        let mode = if has_regex_metacharacters(&grep_text) {
            GrepMode::Regex
        } else {
            GrepMode::PlainText
        };

        let mut result = self.perform_grep(
            &params.query,
            mode,
            max_results,
            params.cursor.as_deref(),
            output_mode,
            None,
        )?;
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// Search file contents for lines matching ANY of multiple patterns (OR logic).
    /// Patterns are literal text — NEVER escape special characters.
    #[tool(
        name = "multi_grep",
        description = "Search file contents for lines matching ANY of multiple patterns (OR logic). IMPORTANT: This returns files where ANY query matches, NOT all patterns. Patterns are literal text — NEVER escape special characters (no \\( \\) \\. etc). Faster than regex alternation for literal text. See server instructions for constraint syntax.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    fn multi_grep(
        &self,
        Parameters(params): Parameters<MultiGrepParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let max_results = normalize_max_results(params.max_results, 20);

        #[cfg(unix)]
        if let Some(result) = self.proxy_multi_grep(&params, max_results) {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        self.bump_activity();
        self.wait_for_scan(SCAN_READY_TIMEOUT)?;

        let mut result = self.multi_grep_inner(params)?;
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// Record that you opened or read a file, boosting its frecency score for future searches.
    /// Call this after reading a file to help fff rank it higher in upcoming searches.
    #[tool(
        name = "record_access",
        description = "Record that you opened or read a file, boosting its frecency score. Call this after reading a file — fff will rank it higher in subsequent find_files and list_recent_files results. Relative or absolute paths are both accepted."
    )]
    fn record_access(
        &self,
        Parameters(params): Parameters<RecordAccessParams>,
    ) -> Result<CallToolResult, ErrorData> {
        #[cfg(unix)]
        if let Some(result) = self.proxy_record_access(&params.path) {
            return result;
        }

        let abs_path = {
            let path = std::path::Path::new(&params.path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                let guard = self.picker.read().map_err(|e| {
                    ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
                })?;
                let picker = guard.as_ref().ok_or_else(|| {
                    ErrorData::internal_error("File picker not initialized", None)
                })?;
                picker.base_path().join(path)
            }
        };

        if let Ok(guard) = self.frecency.read()
            && let Some(tracker) = guard.as_ref()
            && let Err(e) = tracker.track_access(&abs_path)
        {
            tracing::warn!(?abs_path, "record_access track_access failed: {e}");
        }

        Ok(CallToolResult::success(vec![Content::text("ok")]))
    }

    /// List the most recently and frequently accessed files, ranked by frecency.
    #[tool(
        name = "list_recent_files",
        description = "List files ranked by recent access frequency (frecency) — no query needed. Use at session start to understand what's in flight, or to find files you were working on recently. Set dirty_only=true to focus on files with uncommitted changes."
    )]
    fn list_recent_files(
        &self,
        Parameters(params): Parameters<ListRecentFilesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = normalize_max_results(params.max_results, 20);
        let dirty_only = params.dirty_only.unwrap_or(false);

        #[cfg(unix)]
        if let Some(result) =
            self.proxy_list_recent_files(limit, dirty_only, params.base_path.as_deref())
        {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;

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

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No recently accessed files.",
            )]));
        }

        let lines: Vec<String> = items
            .into_iter()
            .map(|(item, score)| {
                format!(
                    "{}{}",
                    item.relative_path(picker),
                    file_suffix(item.git_status, score)
                )
            })
            .collect();

        let mut result = CallToolResult::success(vec![Content::text(lines.join("\n"))]);
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// List all files with uncommitted git changes, grouped by status.
    #[tool(
        name = "get_git_status",
        description = "List all files with uncommitted git changes, grouped by status (modified, staged, untracked, etc.). Use instead of shelling out to `git status`. Set include_clean=true to also see tracked clean files."
    )]
    fn get_git_status(
        &self,
        Parameters(params): Parameters<GetGitStatusParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let include_clean = params.include_clean.unwrap_or(false);

        #[cfg(unix)]
        if let Some(result) = self.proxy_get_git_status(include_clean, params.base_path.as_deref())
        {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;

        use fff::git::format_git_status_opt;
        use fff_ipc::types::WireGitFile;

        let items: Vec<WireGitFile> = picker
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

        if items.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No uncommitted changes.",
            )]));
        }

        let text = format_git_status_wire(&items);
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// List project directories ranked by their most-active files.
    #[tool(
        name = "list_directories",
        description = "List directories that contain indexed files, ranked by the frecency of their most-active child files. Only non-ignored directories with at least one tracked file appear — empty directories and gitignored paths (node_modules/, target/, etc.) are excluded by design. Use to understand which parts of the project are currently active."
    )]
    fn list_directories(
        &self,
        Parameters(params): Parameters<ListDirectoriesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let limit = normalize_max_results(params.max_results, 30);

        #[cfg(unix)]
        if let Some(result) = self.proxy_list_directories(limit, params.base_path.as_deref()) {
            let mut r = result?;
            self.maybe_append_update_notice(&mut r);
            return Ok(r);
        }

        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;

        let mut dirs: Vec<(String, i32)> = picker
            .get_dirs()
            .iter()
            .map(|d| (d.relative_path(picker), d.max_access_frecency()))
            .filter(|(path, _)| !path.is_empty())
            .collect();

        dirs.sort_unstable_by(|(_, a), (_, b)| b.cmp(a));
        dirs.truncate(limit);

        if dirs.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "No directories found.",
            )]));
        }

        let lines: Vec<String> = dirs
            .into_iter()
            .map(|(path, frecency)| format!("{}{}", path, file_suffix(None, frecency)))
            .collect();

        let mut result = CallToolResult::success(vec![Content::text(lines.join("\n"))]);
        self.maybe_append_update_notice(&mut result);
        Ok(result)
    }

    /// List the project roots this fff-mcp instance can search.
    #[tool(
        name = "list_roots",
        description = "List the project roots this fff-mcp instance can search. Returns each registered root as `{ base_path, name?, default }`. The `name` field is present only for roots declared with a name in the config file. Pass either a returned `base_path` or `name` to other tools' `base_path` argument to target that root. Call this first to discover which repos this fff-mcp instance can search."
    )]
    fn list_roots(
        &self,
        Parameters(_params): Parameters<ListRootsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        #[cfg(unix)]
        if let Some(registry) = self.registry.as_ref() {
            let roots: Vec<serde_json::Value> = registry
                .all_with_names()
                .into_iter()
                .map(|(name, p, is_default)| {
                    let mut obj = serde_json::Map::new();
                    obj.insert(
                        "base_path".to_string(),
                        serde_json::Value::String(p.to_string_lossy().into_owned()),
                    );
                    if let Some(n) = name {
                        obj.insert("name".to_string(), serde_json::Value::String(n.to_string()));
                    }
                    obj.insert("default".to_string(), serde_json::Value::Bool(is_default));
                    serde_json::Value::Object(obj)
                })
                .collect();
            let body = serde_json::json!({ "roots": roots });
            return Ok(CallToolResult::success(vec![Content::text(
                body.to_string(),
            )]));
        }
        // Direct (no engine) mode — report the picker's base path as the
        // only known root.
        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let base = guard
            .as_ref()
            .map(|p| p.base_path().to_string_lossy().into_owned())
            .unwrap_or_default();
        let body = serde_json::json!({
            "roots": [{ "base_path": base, "default": true }],
        });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }
}

impl FffServer {
    fn multi_grep_inner(&self, params: MultiGrepParams) -> Result<CallToolResult, ErrorData> {
        let max_results = normalize_max_results(params.max_results, 20);
        let context = params.context.map(|v| v.round() as usize);
        let output_mode = OutputMode::new(params.output_mode.as_deref());

        let file_offset = params
            .cursor
            .as_deref()
            .and_then(|id| self.cursor_store.lock().ok()?.get(id))
            .unwrap_or(0);

        let (options, auto_expand) =
            make_grep_options(output_mode, GrepMode::PlainText, file_offset, context);

        let ctx_lines = options.before_context;
        let constraint_query = params.constraints.as_deref().unwrap_or("");
        let guard = self.picker.read().map_err(|e| {
            ErrorData::internal_error(format!("Failed to acquire picker lock: {e}"), None)
        })?;
        let picker = guard
            .as_ref()
            .ok_or_else(|| ErrorData::internal_error("File picker not initialized", None))?;
        let patterns_refs: Vec<&str> = params.patterns.iter().map(|s| s.as_str()).collect();

        let parser = QueryParser::new(AiGrepConfig);
        let constraints = parser.parse_constraints(constraint_query);

        let result = picker.multi_grep(&patterns_refs, &constraints, &options);
        let file_refs: Vec<&FileItem> = result.files.to_vec();

        if result.matches.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "0 matches.".to_string(),
            )]));
        }

        let mut cs = self.lock_cursors()?;
        let text = &GrepFormatter {
            matches: &result.matches,
            files: &file_refs,
            total_matched: result.matches.len(),
            next_file_offset: result.next_file_offset,
            output_mode,
            max_results,
            show_context: ctx_lines > 0,
            auto_expand_defs: auto_expand,
            picker,
        }
        .format(&mut cs);

        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for FffServer {
    fn get_info(&self) -> ServerInfo {
        let notice = crate::update_check::get_update_notice();
        let instructions = if notice.is_empty() {
            crate::MCP_INSTRUCTIONS.to_string()
        } else {
            format!("{}{}", crate::MCP_INSTRUCTIONS, notice)
        };

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fff", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }
}

fn format_recent_files_wire(items: &[fff_ipc::types::WireSearchResult]) -> String {
    items
        .iter()
        .map(|item| {
            let git_status = item.git_status.map(git2::Status::from_bits_truncate);
            format!(
                "{}{}",
                item.path,
                file_suffix(git_status, item.frecency_score)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_git_status_wire(items: &[fff_ipc::types::WireGitFile]) -> String {
    use std::collections::HashMap;

    const STATUS_ORDER: &[&str] = &[
        "modified",
        "staged_modified",
        "staged_new",
        "staged_deleted",
        "deleted",
        "renamed",
        "untracked",
        "clean",
        "ignored",
    ];

    let mut groups: HashMap<&str, Vec<&str>> = HashMap::new();
    for item in items {
        groups
            .entry(item.status.as_str())
            .or_default()
            .push(item.path.as_str());
    }

    let mut lines: Vec<String> = Vec::new();
    for &status in STATUS_ORDER {
        if let Some(paths) = groups.remove(status) {
            lines.push(format!("{} ({}):", status, paths.len()));
            for path in paths {
                lines.push(format!("  {path}"));
            }
        }
    }
    // any remaining unknown statuses, sorted for determinism
    let mut remaining: Vec<_> = groups.into_iter().collect();
    remaining.sort_by_key(|(k, _)| *k);
    for (status, paths) in remaining {
        lines.push(format!("{status} ({}):", paths.len()));
        for path in paths {
            lines.push(format!("  {path}"));
        }
    }

    lines.join("\n")
}

fn format_directories_wire(items: &[fff_ipc::types::WireDirEntry]) -> String {
    items
        .iter()
        .map(|d| format!("{}{}", d.path, file_suffix(None, d.max_frecency)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// True when master mode is usable. Actively (re)spawns `fff-engine --master`
/// if its socket is gone — so a master that died while the MCP server kept
/// running is recovered here instead of failing every multi-root call until
/// restart. Gates non-default `base_path` calls; multi-root needs master mode.
#[cfg(unix)]
fn master_mode_available() -> bool {
    crate::client::EngineClient::ensure_master().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_max_results_none_uses_default() {
        assert_eq!(normalize_max_results(None, 20), 20);
    }

    #[test]
    fn normalize_max_results_zero_uses_default() {
        // Issue #400: `maxResults: 0` must not return zero items for grep
        // while `find_files` returns the full set. Both tools now map 0 to
        // the default limit.
        assert_eq!(normalize_max_results(Some(0.0), 20), 20);
    }

    #[test]
    fn normalize_max_results_negative_uses_default() {
        assert_eq!(normalize_max_results(Some(-5.0), 20), 20);
    }

    #[test]
    fn normalize_max_results_non_finite_uses_default() {
        assert_eq!(normalize_max_results(Some(f64::NAN), 20), 20);
        assert_eq!(normalize_max_results(Some(f64::INFINITY), 20), 20);
    }

    #[test]
    fn normalize_max_results_rounds_and_clamps() {
        assert_eq!(normalize_max_results(Some(0.4), 20), 1);
        assert_eq!(normalize_max_results(Some(10.0), 20), 10);
        assert_eq!(normalize_max_results(Some(10.7), 20), 11);
    }

    #[test]
    fn grep_params_accepts_pattern_alias() {
        // Issue #311: LLMs flip between `query` and `pattern`; accept both.
        let via_query: GrepParams =
            serde_json::from_str(r#"{"query":"foo"}"#).expect("query field");
        assert_eq!(via_query.query, "foo");

        let via_pattern: GrepParams =
            serde_json::from_str(r#"{"pattern":"foo"}"#).expect("pattern alias");
        assert_eq!(via_pattern.query, "foo");
    }

    #[test]
    fn find_files_params_accepts_pattern_alias() {
        let via_query: FindFilesParams =
            serde_json::from_str(r#"{"query":"foo"}"#).expect("query field");
        assert_eq!(via_query.query, "foo");

        let via_pattern: FindFilesParams =
            serde_json::from_str(r#"{"pattern":"foo"}"#).expect("pattern alias");
        assert_eq!(via_pattern.query, "foo");
    }

    // ── resolve_route: name lookup + bad-input errors ──────────────────────

    #[cfg(unix)]
    fn server_with_registry(reg: crate::registry::RootRegistry) -> FffServer {
        FffServer::new_proxy(Arc::new(crate::pool::ConnectionPool::new()), Arc::new(reg))
    }

    #[cfg(unix)]
    #[test]
    fn resolve_route_unknown_name_returns_invalid_params() {
        use crate::registry::RootRegistry;
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = RootRegistry::with_named(tmp.path(), None, Vec::new());
        let server = server_with_registry(reg);
        let err = server
            .resolve_route(Some("not-a-known-name"))
            .expect_err("must reject");
        assert!(
            err.message
                .contains("neither a registered root name nor an absolute path"),
            "got: {}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_route_relative_path_returns_invalid_params() {
        use crate::registry::RootRegistry;
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = RootRegistry::with_named(tmp.path(), None, Vec::new());
        let server = server_with_registry(reg);
        let err = server
            .resolve_route(Some("./relative"))
            .expect_err("must reject relative paths");
        assert!(
            err.message.contains("absolute path"),
            "got: {}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_route_known_name_returns_path() {
        use crate::registry::RootRegistry;
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = RootRegistry::with_named(tmp.path(), None, vec![(Some("ay".into()), a.clone())]);
        let server = server_with_registry(reg);
        // Without master mode active this returns Err (multi-root requires
        // master), but the error tells us name lookup succeeded — different
        // from the "neither name nor path" error.
        match server.resolve_route(Some("ay")) {
            Ok(Some(p)) => assert_eq!(p, a.canonicalize().unwrap()),
            Ok(None) => panic!("expected resolved path"),
            Err(e) => {
                // Name was recognised — error must be the multi-root master
                // gate, not the "unknown name" message.
                assert!(
                    e.message.contains("master mode"),
                    "expected multi-root gate, got: {}",
                    e.message
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_route_default_name_returns_default_path() {
        use crate::registry::RootRegistry;
        let tmp = tempfile::TempDir::new().unwrap();
        let reg = RootRegistry::with_named(tmp.path(), Some("primary".into()), Vec::new());
        let server = server_with_registry(reg);
        let resolved = server
            .resolve_route(Some("primary"))
            .expect("default-by-name routes back to default")
            .expect("Some path");
        assert_eq!(resolved, tmp.path().canonicalize().unwrap());
    }
}
