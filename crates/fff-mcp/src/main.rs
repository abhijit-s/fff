//! FFF MCP Server — high-performance file finder for AI code assistants.
//!
//! Drop-in replacement for AI code assistant file search tools (Glob/Grep).
//! Provides frecency-ranked, fuzzy-matched, git-aware file finding and
//! code search via the Model Context Protocol (MCP).
//!
//! Uses `fff-core` directly (zero FFI overhead) for all search operations.

#[cfg(unix)]
pub(crate) mod client;
mod cursor;
mod handshake;
mod healthcheck;
mod output;
mod parent;
#[cfg(unix)]
pub(crate) mod pool;
#[cfg(unix)]
mod recovery;
pub(crate) mod registry;
mod server;
mod update_check;

use std::time::{Duration, SystemTime};

use clap::Parser;
use fff::file_picker::FilePicker;
use fff::frecency::FrecencyTracker;
use fff::{FFFMode, SharedFilePicker, SharedFrecency};
use git2::Repository;
use handshake::ProbeTolerantTransport;
use mimalloc::MiMalloc;
use rmcp::ServiceExt;
use rmcp::transport::async_rw::AsyncRwTransport;
use server::FffServer;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub const MCP_INSTRUCTIONS: &str = concat!(
    "FFF is a fast file finder with frecency-ranked results (frequent/recent files first, git-dirty files boosted).\n",
    "\n",
    "## Which Tool Should I Use?\n",
    "\n",
    "- **grep**: DEFAULT tool. Searches file CONTENTS -- definitions, usage, patterns. Use when you have a specific name or pattern.\n",
    "- **find_files**: Explores which files/modules exist for a topic. Use when you DON'T have a specific identifier or LOOKING FOR A FILE.\n",
    "- **multi_grep**: OR logic across multiple patterns. Use for case variants (e.g. ['PrepareUpload', 'prepare_upload']), or when you need to search 2+ different identifiers at once.\n",
    "\n",
    "## Core Rules\n",
    "\n",
    "### 1. Search BARE IDENTIFIERS only\n",
    "Grep matches single lines. Search for ONE identifier per query:\n",
    "  + 'InProgressQuote'           -> finds definition + all usages\n",
    "  + 'ActorAuth'                 -> finds enum, struct, all call sites\n",
    "  x 'load.*metadata.*InProgressQuote' -> regex spanning multiple tokens, 0 results\n",
    "  x 'ctx.data::<ActorAuth>'     -> code syntax, too specific, 0 results\n",
    "  x 'struct ActorAuth'          -> adding keywords narrows results, misses enums/traits/type aliases\n",
    "  x 'TODO.*#\\d+'               -> complex regex, use simple 'TODO' then filter visually\n",
    "\n",
    "### 2. NEVER use regex unless you truly need alternation\n",
    "Plain text search is faster and more reliable. Regex patterns like `.*`, `\\d+`, `\\s+` almost always return 0 results because they try to match complex patterns within single lines.\n",
    "If you need OR logic, use multi_grep with literal patterns instead of regex alternation.\n",
    "\n",
    "### 3. Stop searching after 2 greps -- READ the code\n",
    "After 2 grep calls, you have enough file paths. Read the top result to understand the code.\n",
    "Do NOT keep grepping with variations. More greps != better understanding.\n",
    "\n",
    "### 4. Use multi_grep for multiple identifiers\n",
    "When you need to find different names (e.g. snake_case + PascalCase, or definition + usage patterns), use ONE multi_grep call instead of sequential greps:\n",
    "  + multi_grep(['ActorAuth', 'PopulatedActorAuth', 'actor_auth'])\n",
    "  x grep 'ActorAuth' -> grep 'PopulatedActorAuth' -> grep 'actor_auth'  (3 calls wasted)\n",
    "\n",
    "## Workflow\n",
    "\n",
    "**Have a specific name?** -> grep the bare identifier.\n",
    "**Need multiple name variants?** -> multi_grep with all variants in one call.\n",
    "**Exploring a topic / finding files?** -> find_files.\n",
    "**Got results?** -> Read the top file. Don't grep again.\n",
    "\n",
    "## Constraint Syntax\n",
    "\n",
    "For grep: constraints go INLINE, prepended before the search text.\n",
    "For multi_grep: constraints go in the separate 'constraints' parameter.\n",
    "\n",
    "Constraints MUST match one of these formats:\n",
    "  Extension: '*.rs', '*.{ts,tsx}'\n",
    "  Directory: 'src/', 'quotes/'\n",
    "  Filename: 'schema.rs', 'src/main.rs'\n",
    "  Exclude: '!test/', '!*.spec.ts'\n",
    "\n",
    "! Bare words without extensions are NOT constraints. 'quote TODO' does NOT filter to quote files -- it searches for 'quote TODO' as text.\n",
    "  + 'schema.rs TODO'   -> searches for 'TODO' in files schema.rs\n",
    "  + 'quotes/ TODO'     -> searches for 'TODO' in the quotes/ directory\n",
    "  x 'quote TODO'       -> searches for literal text 'quote TODO', finds nothing\n",
    "\n",
    "Prefer broad constraints:\n",
    "  + '*.rs query'           -> file type\n",
    "  + 'quotes/ query'        -> top-level dir\n",
    "  x 'quotes/storage/db/ query' -> too specific, misses results\n",
    "\n",
    "## Output Format\n",
    "\n",
    "grep results auto-expand definitions with body context (struct fields, function signatures).\n",
    "This often provides enough information WITHOUT a follow-up Read call.\n",
    "Lines marked with | are definition body context. [def] marks definition files.\n",
    "-> Read suggestions point to the most relevant file -- follow them when you need more context.\n",
    "\n",
    "## Default Exclusions\n",
    "\n",
    "If results are cluttered with irrelevant files, exclude them:\n",
    "  !tests/ - exclude tests directory\n",
    "  !*.spec.ts - exclude test files\n",
    "  !generated/ - exclude generated code\n",
    "\n",
    "## Context Tools (no query needed)\n",
    "\n",
    "Use these when you don't have a specific identifier to search for:\n",
    "\n",
    "- **list_recent_files**: Show which files have been opened/modified most recently. Use at session start to understand what's in flight, or to find the file you were just editing.\n",
    "- **get_git_status**: Show all uncommitted changes grouped by status (modified, staged, untracked…). Use instead of shelling out to `git status`.\n",
    "- **list_directories**: Show non-ignored directories (with at least one indexed file) ranked by activity. Empty dirs and gitignored paths are absent by design — absence does not mean the directory doesn't exist.\n",
    "- **record_access**: Tell fff you opened a file. The file will rank higher in future find_files and list_recent_files results. Call this after reading a key file.",
);

/// FFF MCP Server - a high performance & accuracy file finder for AI code assistants.
#[derive(Parser)]
#[command(name = "fff-mcp", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")"))]
pub(crate) struct Args {
    /// Base directory to index. Defaults to `[mcp].default` from config, then
    /// the current working directory.
    #[arg(value_name = "PATH")]
    base_path: Option<String>,

    /// Pre-register an additional project root for multi-root mode (repeatable).
    /// Tools accept an optional `base_path` argument matching one of these.
    /// Requires master mode.
    #[arg(long = "root", value_name = "PATH")]
    root: Vec<std::path::PathBuf>,

    /// Path to a config file, overriding the default `~/.config/fff/config.toml`.
    /// Roots live under its `[mcp]` section; `--root` flags merge on top.
    #[arg(long = "config", value_name = "PATH")]
    config_file: Option<std::path::PathBuf>,

    /// Path to the frecency database.
    #[arg(long = "frecency-db")]
    frecency_db_path: Option<String>,

    /// Path to the query history database.
    #[arg(long = "history-db")]
    #[allow(dead_code)]
    history_db_path: Option<String>,

    /// Path-shape hint for per-session log files.
    /// Each fff-mcp startup writes a fresh sibling file `<stem>+<UTC-timestamp>+<pid>.<ext>`
    #[arg(long = "log-file")]
    log_file: Option<String>,

    /// Log level (e.g. trace, debug, info, warn, error).
    #[arg(long = "log-level")]
    log_level: Option<String>,

    /// Disable automatic update checks on startup.
    #[arg(long = "no-update-check")]
    no_update_check: bool,

    /// Disable eager mmap warmup after the initial scan. Grep results will
    /// still work (files are mmap'd lazily on first access), but the first
    /// search may be slightly slower. Useful on very large repos where the
    /// warmup would consume too many kernel resources.
    #[arg(long = "no-warmup")]
    no_warmup: bool,

    /// Disable the content index built after the initial scan.
    /// This makes grep calls slower but consumes less RAM (recommended to not turn off)
    #[arg(long = "no-content-indexing")]
    no_content_indexing: bool,

    /// Explicitly enable content indexing even when `--no-warmup` is set.
    #[arg(long = "content-indexing")]
    content_indexing: bool,

    /// Disable the background file-system watcher. Files are scanned once
    /// at startup but not monitored for changes.
    #[arg(long = "no-watch")]
    no_watch: bool,

    /// Maximum number of files whose content is kept persistently in memory.
    /// Files beyond this limit are still searchable via temporary mmaps that
    /// are released after each grep. Defaults to 30 000.
    /// Also settable via the FFF_MAX_CACHED_FILES environment variable.
    #[arg(long = "max-cached-files", env = "FFF_MAX_CACHED_FILES")]
    max_cached_files: Option<usize>,

    /// Follow symlinks during scan and watcher walks. Off by default —
    /// enabling on cyclic symlink layouts can wedge the watcher.
    #[arg(long = "follow-symlinks")]
    follow_symlinks: bool,

    /// Allow indexing the user's home directory. FFF refuses to init in `~`
    /// unless this is set. Also settable via FFF_ENABLE_HOME_SCAN=1.
    #[arg(
        long = "enable-home-scan",
        env = "FFF_ENABLE_HOME_SCAN",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    enable_home_scan: bool,

    /// Allow indexing the filesystem root, off by default for the same reason.
    /// Also settable via FFF_ENABLE_ROOT_SCAN=1.
    #[arg(
        long = "enable-root-scan",
        env = "FFF_ENABLE_ROOT_SCAN",
        num_args = 0..=1,
        default_missing_value = "true",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    enable_root_scan: bool,

    /// Run a health check and print diagnostic information, then exit.
    #[arg(long = "healthcheck")]
    pub(crate) healthcheck: bool,

    /// Hot-reload the running fff-engine's log level and exit.
    /// Accepts any RUST_LOG-style string: "debug", "info", "fff_engine=debug,info".
    #[arg(long = "set-log-level", value_name = "LEVEL")]
    pub(crate) set_log_level: Option<String>,

    /// Print a shell completion script to stdout and exit.
    #[arg(long = "completions", value_name = "SHELL")]
    completions: Option<clap_complete::Shell>,

    /// Timeout of inactivity after which fff mcp will be exited. Even if the parent process
    /// is alive we don't want to occupy resources on index and file watches if fff is unused
    #[arg(
        long = "idle-timeout-secs",
        env = "FFF_MCP_IDLE_TIMEOUT_SECS",
        default_value_t = 60 * 60
    )]
    idle_timeout_secs: u64,
}

/// Assemble a `RootRegistry` from CLI args + an optional `--config` TOML.
///
/// `base_path` is already resolved (CLI > config.default > cwd). Named
/// roots from the config file populate the registry alongside `--root`
/// extras; duplicates by canonical path are dropped.
fn build_registry(
    base_path: &std::path::Path,
    cli_extras: &[std::path::PathBuf],
    mcp: &fff_ipc::config::McpConfig,
) -> registry::RootRegistry {
    // Default name: the explicit `default` when it names a root, else a
    // canonical-path match against the declared roots.
    let default_name = if let Some(def) = mcp.default.as_deref()
        && mcp.roots.iter().any(|r| r.name.as_deref() == Some(def))
    {
        Some(def.to_string())
    } else {
        name_for_path(mcp, base_path)
    };

    let mut extras: Vec<(Option<String>, std::path::PathBuf)> = Vec::new();
    for r in &mcp.roots {
        extras.push((r.name.clone(), r.path.clone()));
    }
    for p in cli_extras {
        extras.push((None, p.clone()));
    }

    registry::RootRegistry::with_named(base_path, default_name, extras)
}

// Name of the declared root whose canonical path matches `path`. Both sides are
// canonicalized: base_path arrives git-discovered/symlink-resolved, so a literal
// compare would silently miss.
fn name_for_path(mcp: &fff_ipc::config::McpConfig, path: &std::path::Path) -> Option<String> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    mcp.roots
        .iter()
        .find(|r| std::fs::canonicalize(&r.path).unwrap_or_else(|_| r.path.clone()) == target)
        .and_then(|r| r.name.clone())
}

/// Merge CLI args with config file, then apply hardcoded defaults for anything
/// still unset. Priority: CLI > config > hardcoded default.
fn resolve_defaults(args: &mut Args, cfg: &fff_ipc::config::FffConfig) {
    // log_level: CLI > config > "info"
    if args.log_level.is_none() {
        args.log_level = Some(cfg.log.level.clone());
    }

    // log_file: CLI > config > $XDG_CACHE_HOME/fff_mcp.log
    if args.log_file.is_none() {
        args.log_file = Some(cfg.log.file.clone().unwrap_or_else(|| {
            fff_ipc::xdg_cache_dir()
                .join("fff_mcp.log")
                .to_string_lossy()
                .into_owned()
        }));
    }

    // max_cached_files: CLI > config
    if args.max_cached_files.is_none() {
        args.max_cached_files = cfg.index.max_cached_files;
    }

    // Booleans: CLI flag OR config flag (either can disable)
    args.no_watch = args.no_watch || cfg.index.no_watch;
    args.no_warmup = args.no_warmup || cfg.index.no_warmup;

    // Ensure parent dirs exist for any explicitly provided db paths
    for path in [&args.frecency_db_path, &args.history_db_path]
        .into_iter()
        .flatten()
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = Args::parse();

    if let Some(shell) = args.completions {
        use clap::CommandFactory;
        let mut cmd = Args::command();
        clap_complete::generate(shell, &mut cmd, "fff-mcp", &mut std::io::stdout());
        return Ok(());
    }

    // `--config` names an explicit file (fatal on failure); otherwise load the
    // default XDG path best-effort. One `cfg` feeds everything downstream.
    let cfg = match args.config_file.as_deref() {
        Some(path) => fff_ipc::config::load_from(path).unwrap_or_else(|e| {
            eprintln!("fff-mcp: {e}");
            std::process::exit(1);
        }),
        None => fff_ipc::config::load(),
    };
    if let Err(e) = cfg.mcp.validate() {
        eprintln!("fff-mcp: {e}");
        std::process::exit(1);
    }
    if args.config_file.is_some() && cfg.mcp.roots.is_empty() {
        eprintln!("fff-mcp: --config given but no [mcp].roots declared");
    }
    resolve_defaults(&mut args, &cfg);

    if args.healthcheck {
        return healthcheck::run_healthcheck(&args);
    }

    #[cfg(unix)]
    if let Some(ref level) = args.set_log_level {
        let base_path = args.base_path.as_deref().unwrap_or(".");
        let mut engine = client::EngineClient::connect(std::path::Path::new(base_path))
            .map_err(|e| format!("Could not connect to fff-engine: {e}"))?;
        match engine.set_log_level(level) {
            Ok(fff_ipc::types::SearchResponse::Ack) => {
                println!("fff-engine log level set to {level:?}");
            }
            Ok(fff_ipc::types::SearchResponse::Error(e)) => {
                eprintln!("fff-engine error: {e}");
                std::process::exit(1);
            }
            Ok(_) => eprintln!("Unexpected response"),
            Err(e) => {
                eprintln!("IPC error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let log_file = args.log_file.as_deref().unwrap_or("");
    if let Err(e) = fff::log::init_tracing(log_file, args.log_level.as_deref(), None) {
        eprintln!("Warning: Failed to init tracing: {}", e);
    }

    // base_path: positional arg > [mcp].default > cwd.
    let base_path = args
        .base_path
        .clone()
        .or_else(|| {
            cfg.mcp
                .default_path()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        });

    let base_path = match Repository::discover(&base_path) {
        Ok(repo) => {
            if let Some(workdir) = repo.workdir() {
                let git_root = workdir.to_string_lossy().to_string();
                tracing::info!("Discovered git root: {}", git_root);
                git_root
            } else {
                tracing::info!("Git repository is bare, using base path: {}", base_path);
                base_path
            }
        }
        Err(_) => {
            tracing::info!(
                "No git repository found, indexing from base path: {}",
                base_path
            );
            base_path
        }
    };

    // Captured BEFORE the initialize handshake: a client that dies right after
    // handshaking would otherwise have reparented us to init by the time we
    // looked, so ppid <= 1 would read as "detection unavailable" rather than
    // "orphaned" -- and with no idle timeout that means running forever.
    let parent_watcher = parent::ParentWatcher::new();
    match &parent_watcher {
        Some(watcher) => tracing::info!(
            "Watching parent process (pid {}); will exit when it dies",
            watcher.parent_pid()
        ),
        None => tracing::warn!(
            "Parent process liveness detection unavailable; idle timeout will exit unconditionally"
        ),
    }

    // ── Unix proxy path: connect to fff-engine daemon ─────────────────────────
    #[cfg(unix)]
    {
        let base_path_ref = std::path::Path::new(&base_path);
        match client::EngineClient::connect(base_path_ref) {
            Ok(engine_client) => {
                if !args.no_update_check {
                    update_check::spawn_update_check();
                }
                let registry = build_registry(base_path_ref, &args.root, &cfg.mcp);
                let pool = pool::ConnectionPool::new();
                pool.insert(base_path_ref, engine_client);
                let server =
                    FffServer::new_proxy(std::sync::Arc::new(pool), std::sync::Arc::new(registry));
                let last_activity = server.last_activity();
                let stdio = AsyncRwTransport::new_server(tokio::io::stdin(), tokio::io::stdout());
                let transport = ProbeTolerantTransport::new(stdio);
                let service = server
                    .serve(transport)
                    .await
                    .map_err(|e| format!("Failed to start MCP server: {}", e))?;
                spawn_lifecycle_watchdog(parent_watcher, last_activity, args.idle_timeout_secs);
                service.waiting().await?;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to connect to fff-engine ({e}), falling back to direct mode"
                );
            }
        }
    }

    // ── Direct path (Windows, or Unix fallback when engine unavailable) ───────
    let shared_picker = SharedFilePicker::default();
    let shared_frecency = SharedFrecency::default();
    if let Some(frecency_db_path) = args.frecency_db_path {
        match FrecencyTracker::open(&frecency_db_path) {
            Ok(tracker) => {
                let _ = shared_frecency.init(tracker);
            }
            Err(e) => {
                eprintln!("Warning: Failed to init frecency db: {}", e);
            }
        }
    }

    // Content indexing follows warmup by default (backward compat), unless
    // the user explicitly opts in via --content-indexing or out via
    // --no-content-indexing.
    let enable_content_indexing = if args.content_indexing {
        true
    } else if args.no_content_indexing {
        false
    } else {
        !args.no_warmup
    };

    // Initialize file picker (spawns background scan + watcher)
    FilePicker::new_with_shared_state(
        shared_picker.clone(),
        shared_frecency.clone(),
        fff::FilePickerOptions {
            base_path,
            enable_mmap_cache: !args.no_warmup,
            enable_content_indexing,
            watch: !args.no_watch,
            mode: FFFMode::Ai,
            cache_budget: args
                .max_cached_files
                .map(fff::ContentCacheBudget::new_for_repo),
            follow_symlinks: args.follow_symlinks,
            enable_home_dir_scanning: args.enable_home_scan,
            enable_fs_root_scanning: args.enable_root_scan,
            // Standalone MCP path has no per-root ignore config; spread fills
            // ignore_globs (and any future upstream fields) with defaults.
            ..Default::default()
        },
    )
    .map_err(|e| format!("Failed to init file picker: {}", e))?;

    if !args.no_update_check {
        update_check::spawn_update_check();
    }

    // Create and start the MCP server
    let server = FffServer::new(shared_picker.clone(), shared_frecency);
    let last_activity = server.last_activity();
    let idle_timeout_secs = args.idle_timeout_secs;

    // Wait for initial scan in background — don't block server startup
    let picker_clone_for_scan = shared_picker.clone();
    tokio::task::spawn_blocking(move || {
        let start = std::time::Instant::now();
        loop {
            let is_scanning = picker_clone_for_scan
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|p| p.is_scan_active()))
                .unwrap_or(true);

            if !is_scanning {
                tracing::info!("Initial scan completed in {:?}", start.elapsed());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });

    const STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let stdio = AsyncRwTransport::new_server(tokio::io::stdin(), tokio::io::stdout());
    let transport = ProbeTolerantTransport::new(stdio);
    let service = match tokio::time::timeout(STARTUP_TIMEOUT, server.serve(transport)).await {
        Ok(res) => res.map_err(|e| format!("Failed to start MCP server: {}", e))?,
        Err(_) => {
            return Err("MCP initialize handshake did not complete within 60s".into());
        }
    };

    spawn_lifecycle_watchdog(parent_watcher, last_activity, idle_timeout_secs);

    let picker_for_shutdown = shared_picker.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        if let Ok(mut guard) = picker_for_shutdown.write()
            && let Some(ref mut picker) = *guard
        {
            picker.stop_background_monitor();
        }
        std::process::exit(0);
    });

    service.waiting().await?;

    if let Ok(mut guard) = shared_picker.write()
        && let Some(ref mut picker) = *guard
    {
        picker.stop_background_monitor();
    }

    Ok(())
}

/// Exit when the spawning client dies, or (absent a client to watch) when idle.
///
/// Both serve paths need this: the proxy path used to return straight into
/// `service.waiting()` with no watcher at all, so an orphaned proxy-mode
/// server stayed alive forever once its client was gone.
fn spawn_lifecycle_watchdog(
    parent_watcher: Option<parent::ParentWatcher>,
    last_activity: std::sync::Arc<std::sync::atomic::AtomicU64>,
    idle_timeout_secs: u64,
) {
    if idle_timeout_secs > 0 || parent_watcher.is_some() {
        last_activity.store(
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );

        let last_activity_for_watchdog = last_activity.clone();
        tokio::spawn(async move {
            let tick = watchdog_interval();
            loop {
                tokio::time::sleep(tick).await;

                if let Some(ref watcher) = parent_watcher {
                    if !watcher.parent_alive() {
                        tracing::info!(
                            "Parent process (pid {}) exited, shutting down",
                            watcher.parent_pid()
                        );
                        flush_logs_and_exit().await;
                    }
                    // Parent is alive: it owns our lifecycle, never exit on idle
                    // Clients like Codex do not restart MCP servers @see #703
                    continue;
                }

                if idle_timeout_secs == 0 {
                    continue;
                }

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let last = last_activity_for_watchdog.load(std::sync::atomic::Ordering::Relaxed);
                if now.saturating_sub(last) >= idle_timeout_secs {
                    tracing::info!(?idle_timeout_secs, "Exiting due to inactivity",);
                    flush_logs_and_exit().await;
                }
            }
        });
    }
}

// Tracing appender is non blocking, to get full log give it some time before hard exit
async fn flush_logs_and_exit() -> ! {
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    std::process::exit(0);
}

fn watchdog_interval() -> Duration {
    if cfg!(debug_assertions)
        && let Some(milliseconds) = std::env::var("FFF_MCP_TEST_WATCHDOG_INTERVAL_MS")
            .ok()
            .and_then(|value| value.parse().ok())
    {
        return Duration::from_millis(milliseconds);
    }
    Duration::from_secs(60)
}
