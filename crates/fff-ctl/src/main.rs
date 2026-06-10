//! `fffctl` — operator CLI for fff-engine daemons.
//!
//! Prefers the master management protocol when master is running.
//! Falls back to legacy per-root lockfile scanning when master is absent.

#[cfg(unix)]
use std::io::{BufReader, BufWriter};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use clap::{Parser, Subcommand};
use fff_ipc::lockfile::{self, Lockfile};
use fff_ipc::routing::RootEntry;
use fff_ipc::types::{MasterRequest, MasterResponse, WorkerInfo};
use fff_ipc::{master_lockfile_path, master_socket_path, routing_table_path};
#[cfg(unix)]
use fff_ipc::{read_message_sync, write_message_sync};
use serde::Serialize;

#[derive(Parser, Debug)]
#[command(
    name = "fffctl",
    version,
    about = "Manage fff-engine daemons",
    long_about = None,
)]
struct Cli {
    /// Emit machine-readable JSON instead of human-readable text.
    #[arg(long, short = 'j', global = true)]
    json: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List all running daemons (master + workers when master is active).
    List,
    /// Show resolved paths (socket, lockfile, frecency dir, log) for a base-path.
    Paths {
        /// Project root the daemon would (or does) serve.
        #[arg(value_name = "BASE_PATH")]
        base_path: PathBuf,
    },
    /// Query which worker would handle a base-path (read-only).
    Status {
        /// Project root served by the daemon.
        #[arg(value_name = "BASE_PATH")]
        base_path: PathBuf,
    },
    /// Stop daemons. With --all, stops master (which propagates to workers).
    Stop {
        /// Project root served by the daemon. Mutually exclusive with --all.
        #[arg(value_name = "BASE_PATH", conflicts_with = "all")]
        base_path: Option<PathBuf>,
        /// Stop every running daemon (sends SIGTERM to master).
        #[arg(long, conflicts_with = "base_path")]
        all: bool,
        /// Seconds to wait for graceful exit before SIGKILL. 0 disables KILL.
        #[arg(long, default_value_t = 5)]
        timeout: u64,
    },
    /// Remove stale lockfiles, orphan sockets, and unreferenced frecency dirs.
    Clean {
        /// Print actions without performing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show status of a specific worker by index.
    WorkerStatus {
        /// Worker index.
        #[arg(value_name = "INDEX")]
        index: u32,
    },
    /// List all workers managed by the master.
    ListWorkers,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let exit = match cli.command {
        Cmd::List => cmd_list(json),
        Cmd::ListWorkers => cmd_list_workers(json),
        Cmd::Paths { base_path } => cmd_paths(&base_path, json),
        Cmd::Status { base_path } => cmd_status(&base_path, json),
        Cmd::WorkerStatus { index } => cmd_worker_status(index, json),
        Cmd::Stop {
            base_path,
            all,
            timeout,
        } => cmd_stop(
            base_path.as_deref(),
            all,
            Duration::from_secs(timeout),
            json,
        ),
        Cmd::Clean { dry_run } => cmd_clean(dry_run, json),
    };
    std::process::exit(exit);
}

// ─────────────────────────────────────────────────────────────────────────────
// JSON output structs

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum ListJson<'a> {
    Master {
        master_pid: u32,
        worker_count: usize,
        workers: &'a [WorkerInfo],
    },
    Legacy {
        daemons: Vec<LegacyDaemonJson<'a>>,
    },
    None {
        daemons: [u8; 0],
    },
}

#[derive(Serialize)]
struct LegacyDaemonJson<'a> {
    pid: u32,
    state: &'static str,
    slug: &'a str,
    base_path: Option<String>,
}

#[derive(Serialize)]
struct WorkerListJson<'a> {
    workers: &'a [WorkerInfo],
}

#[derive(Serialize)]
struct ErrorJson<'a> {
    error: &'a str,
}

#[derive(Serialize)]
struct StatusHitJson<'a> {
    base_path: String,
    worker: &'a WorkerInfo,
}

#[derive(Serialize)]
struct StatusMissJson {
    base_path: String,
    error: String,
}

#[derive(Serialize)]
struct PathsJson {
    base_path: String,
    slug: String,
    socket: String,
    lockfile: String,
    frecency: String,
    log: String,
    master_sock: String,
    master_lock: String,
    routing: String,
}

#[derive(Serialize)]
struct StopOkJson {
    ok: bool,
    stopped: usize,
}

#[derive(Serialize)]
struct CleanJson {
    removed: Vec<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

fn print_json<T: Serialize>(value: &T) {
    match serde_json::to_string(value) {
        Ok(s) => println!("{s}"),
        Err(e) => println!("{{\"error\":\"json serialization failed: {e}\"}}"),
    }
}

fn print_error_json(msg: &str) {
    print_json(&ErrorJson { error: msg });
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands

fn cmd_list(json: bool) -> i32 {
    // Try master management protocol first.
    if let Some(workers) = master_request_list() {
        let master_lock = master_lockfile_path();
        let master_pid = lockfile::read(&master_lock).map(|l| l.pid).unwrap_or(0);
        if json {
            print_json(&ListJson::Master {
                master_pid,
                worker_count: workers.len(),
                workers: &workers,
            });
            return 0;
        }
        println!("master PID: {master_pid}  workers: {}", workers.len());
        println!("{:<6}  {:<7}  {:<8}  SOCKET", "INDEX", "PID", "ROOTS");
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
        return 0;
    }

    // Legacy fallback: per-root lockfile scan.
    let daemons = discover_daemons();
    if json {
        if daemons.is_empty() {
            print_json(&ListJson::None { daemons: [] });
        } else {
            let entries: Vec<LegacyDaemonJson> = daemons
                .iter()
                .map(|d| LegacyDaemonJson {
                    pid: d.lock.pid,
                    state: if d.lock.is_alive() { "live" } else { "stale" },
                    slug: &d.slug,
                    base_path: d.lock.base_path.as_deref().map(|p| p.display().to_string()),
                })
                .collect();
            print_json(&ListJson::Legacy { daemons: entries });
        }
        return 0;
    }

    eprintln!("Note: master not running, showing legacy per-root daemon list");
    if daemons.is_empty() {
        println!("No fff-engine daemons running.");
        return 0;
    }
    println!("{:<10}  {:<7}  {:<16}  BASE-PATH", "PID", "STATE", "SLUG");
    for d in &daemons {
        let base = d
            .lock
            .base_path
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        println!(
            "{:<10}  {:<7}  {:<16}  {base}",
            d.lock.pid,
            if d.lock.is_alive() { "live" } else { "stale" },
            d.slug
        );
    }
    0
}

fn cmd_list_workers(json: bool) -> i32 {
    match master_request(MasterRequest::ListWorkers) {
        Some(MasterResponse::WorkerList { workers }) => {
            if json {
                print_json(&WorkerListJson { workers: &workers });
                return 0;
            }
            println!("{:<6}  {:<7}  {:<8}  SOCKET", "INDEX", "PID", "ROOTS");
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
            0
        }
        Some(MasterResponse::Error(e)) => {
            if json {
                print_error_json(&e);
            } else {
                eprintln!("master error: {e}");
            }
            1
        }
        None => {
            if json {
                print_error_json("master not running");
            } else {
                eprintln!("master not running");
            }
            1
        }
        _ => {
            if json {
                print_error_json("unexpected response");
            } else {
                eprintln!("unexpected response");
            }
            1
        }
    }
}

fn cmd_paths(base_path: &Path, json: bool) -> i32 {
    let slug = fff_ipc::base_path_slug(base_path);
    let socket = fff_ipc::socket_path(base_path);
    let lockfile = fff_ipc::lockfile_path(base_path);
    let frecency = fff_ipc::xdg_data_dir()
        .join("fff")
        .join("frecency")
        .join(&slug);
    let log = fff_ipc::log_path(base_path);
    let master_sock = master_socket_path();
    let master_lock = master_lockfile_path();
    let routing = routing_table_path();

    if json {
        print_json(&PathsJson {
            base_path: base_path.display().to_string(),
            slug,
            socket: socket.display().to_string(),
            lockfile: lockfile.display().to_string(),
            frecency: frecency.display().to_string(),
            log: log.display().to_string(),
            master_sock: master_sock.display().to_string(),
            master_lock: master_lock.display().to_string(),
            routing: routing.display().to_string(),
        });
        return 0;
    }

    println!("base_path     : {}", base_path.display());
    println!("slug          : {slug}");
    println!("socket        : {}", socket.display());
    println!("lockfile      : {}", lockfile.display());
    println!("frecency      : {}", frecency.display());
    println!("log           : {}", log.display());
    println!("master.sock   : {}", master_sock.display());
    println!("master.lock   : {}", master_lock.display());
    println!("routing.json  : {}", routing.display());
    0
}

fn cmd_status(base_path: &Path, json: bool) -> i32 {
    // Use RouteInfo (read-only) when master is running.
    if let Some(resp) = master_request(MasterRequest::RouteInfo {
        base_path: base_path.to_string_lossy().into(),
    }) {
        match resp {
            MasterResponse::WorkerInfo(info) => {
                if json {
                    print_json(&StatusHitJson {
                        base_path: base_path.display().to_string(),
                        worker: &info,
                    });
                    return 0;
                }
                println!(
                    "Route for {}: worker-{} (pid={}, roots={})",
                    base_path.display(),
                    info.index,
                    info.pid,
                    info.root_count()
                );
                println!("  socket: {}", info.socket_path);
                return 0;
            }
            MasterResponse::Error(e) => {
                if json {
                    print_json(&StatusMissJson {
                        base_path: base_path.display().to_string(),
                        error: e,
                    });
                    return 0;
                }
                println!("{} → {e}", base_path.display());
                return 0;
            }
            _ => {}
        }
    }

    // Legacy fallback.
    let lock_path = fff_ipc::lockfile_path(base_path);
    match lockfile::read(&lock_path) {
        Some(lock) if lock.is_alive() => {
            if json {
                let info = WorkerInfo {
                    index: 0,
                    socket_path: fff_ipc::socket_path(base_path).display().to_string(),
                    roots: vec![RootEntry {
                        slug: fff_ipc::base_path_slug(base_path),
                        base_path: base_path.display().to_string(),
                    }],
                    pid: lock.pid,
                };
                print_json(&StatusHitJson {
                    base_path: base_path.display().to_string(),
                    worker: &info,
                });
                return 0;
            }
            println!(
                "fff-engine for {} is running (singleton).",
                base_path.display()
            );
            println!("  PID: {}  lock: {}", lock.pid, lock_path.display());
            0
        }
        Some(lock) => {
            if json {
                print_json(&StatusMissJson {
                    base_path: base_path.display().to_string(),
                    error: format!("stale PID {}", lock.pid),
                });
                return 1;
            }
            eprintln!(
                "fff-engine for {} is NOT running (stale PID {}).",
                base_path.display(),
                lock.pid
            );
            1
        }
        None => {
            if json {
                print_json(&StatusMissJson {
                    base_path: base_path.display().to_string(),
                    error: "no lockfile".into(),
                });
                return 1;
            }
            eprintln!(
                "fff-engine for {} is NOT running (no lockfile).",
                base_path.display()
            );
            1
        }
    }
}

fn cmd_worker_status(index: u32, json: bool) -> i32 {
    match master_request(MasterRequest::WorkerStatus { index }) {
        Some(MasterResponse::WorkerInfo(info)) => {
            if json {
                print_json(&info);
                return 0;
            }
            println!(
                "worker-{}: pid={} roots={}",
                info.index,
                info.pid,
                info.root_count()
            );
            println!("  socket: {}", info.socket_path);
            for root in &info.roots {
                let path_display = if root.base_path.is_empty() {
                    "<unknown>"
                } else {
                    root.base_path.as_str()
                };
                println!("  {path_display}  (slug: {})", root.slug);
            }
            0
        }
        Some(MasterResponse::Error(e)) => {
            if json {
                print_error_json(&e);
            } else {
                eprintln!("master error: {e}");
            }
            1
        }
        None => {
            if json {
                print_error_json("master not running");
            } else {
                eprintln!("master not running");
            }
            1
        }
        _ => {
            if json {
                print_error_json("unexpected response");
            } else {
                eprintln!("unexpected response");
            }
            1
        }
    }
}

fn cmd_stop(base_path: Option<&Path>, all: bool, timeout: Duration, json: bool) -> i32 {
    if all {
        // Prefer stopping via master (propagates SIGTERM to all workers).
        let master_lock = master_lockfile_path();
        if let Some(lock) = lockfile::read(&master_lock)
            && lock.is_alive()
        {
            #[cfg(unix)]
            {
                let pid = lock.pid as libc::pid_t;
                let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
                if rc == 0 {
                    if json {
                        print_json(&StopOkJson {
                            ok: true,
                            stopped: 1,
                        });
                    } else {
                        println!("Sent SIGTERM to master pid={}", lock.pid);
                    }
                    let deadline = Instant::now() + timeout;
                    while Instant::now() < deadline && lock.is_alive() {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    return 0;
                }
            }
            #[cfg(not(unix))]
            {
                if json {
                    print_error_json("fffctl stop is not supported on this platform");
                } else {
                    eprintln!("fffctl stop is not supported on this platform.");
                }
                return 1;
            }
        }
        // Legacy fallback: stop all per-root daemons.
        let targets: Vec<_> = discover_daemons()
            .into_iter()
            .filter(|d| d.lock.is_alive())
            .collect();
        if targets.is_empty() {
            if json {
                print_json(&StopOkJson {
                    ok: true,
                    stopped: 0,
                });
            } else {
                println!("No live daemons to stop.");
            }
            return 0;
        }
        let mut failures = 0;
        let mut stopped = 0usize;
        for d in targets {
            let label = d
                .lock
                .base_path
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("slug={}", d.slug));
            match stop_daemon(&d, timeout) {
                Ok(()) => {
                    stopped += 1;
                    if !json {
                        println!("Stopped PID {} ({label})", d.lock.pid);
                    }
                }
                Err(e) => {
                    if !json {
                        eprintln!("Failed to stop PID {} ({label}): {e}", d.lock.pid);
                    }
                    failures += 1;
                }
            }
        }
        if json {
            if failures > 0 {
                print_error_json(&format!(
                    "stopped {stopped} daemon(s), {failures} failure(s)"
                ));
            } else {
                print_json(&StopOkJson { ok: true, stopped });
            }
        }
        return if failures > 0 { 1 } else { 0 };
    }

    if let Some(bp) = base_path {
        // Try master StopWorker first.
        let ring_index = master_request(MasterRequest::RouteInfo {
            base_path: bp.to_string_lossy().into(),
        });
        if let Some(MasterResponse::WorkerInfo(info)) = ring_index
            && let Some(MasterResponse::Ack) =
                master_request(MasterRequest::StopWorker { index: info.index })
        {
            if json {
                print_json(&StopOkJson {
                    ok: true,
                    stopped: 1,
                });
            } else {
                println!("Stopped worker-{} for {}", info.index, bp.display());
            }
            return 0;
        }

        // Legacy fallback.
        let lock_path = fff_ipc::lockfile_path(bp);
        match lockfile::read(&lock_path) {
            Some(lock) if lock.is_alive() => {
                let d = Daemon {
                    slug: fff_ipc::base_path_slug(bp),
                    lock,
                    lockfile_path: lock_path,
                };
                match stop_daemon(&d, timeout) {
                    Ok(()) => {
                        if json {
                            print_json(&StopOkJson {
                                ok: true,
                                stopped: 1,
                            });
                        } else {
                            println!("Stopped PID {}", d.lock.pid);
                        }
                        0
                    }
                    Err(e) => {
                        if json {
                            print_error_json(&format!("Failed: {e}"));
                        } else {
                            eprintln!("Failed: {e}");
                        }
                        1
                    }
                }
            }
            _ => {
                if json {
                    print_error_json(&format!("No live daemon for {}", bp.display()));
                } else {
                    eprintln!("No live daemon for {}", bp.display());
                }
                1
            }
        }
    } else {
        if json {
            print_error_json("Specify a base-path or pass --all.");
        } else {
            eprintln!("Specify a base-path or pass --all.");
        }
        2
    }
}

fn cmd_clean(dry_run: bool, json: bool) -> i32 {
    let mut removed_paths: Vec<String> = Vec::new();
    let mut removed_master = 0usize;
    let mut removed_locks = 0;
    let mut removed_sockets = 0;
    let mut removed_logs = 0;

    // ── Master + worker artifacts ─────────────────────────────────────
    let master_lock = master_lockfile_path();
    let master_alive = lockfile::read(&master_lock).is_some_and(|l| l.is_alive());
    if master_alive {
        if !json {
            println!(
                "Note: master is running; skipping master artifacts (use `fffctl stop --all` first)."
            );
        }
    } else {
        removed_master = clean_master_artifacts(dry_run, json, &mut removed_paths);
    }

    // ── Legacy per-root artifacts ─────────────────────────────────────
    for d in discover_daemons() {
        if d.lock.is_alive() {
            continue;
        }
        if json {
            removed_paths.push(d.lockfile_path.display().to_string());
        } else {
            let action = if dry_run { "would remove" } else { "removing" };
            println!(
                "{action} stale lockfile: {} (PID {} dead)",
                d.lockfile_path.display(),
                d.lock.pid
            );
        }
        if !dry_run {
            let _ = std::fs::remove_file(&d.lockfile_path);
        }
        removed_locks += 1;
    }

    let cache = fff_ipc::xdg_cache_dir().join("fff");
    let lock_dir = cache.join("locks");

    // Orphan sockets (no matching live lockfile)
    let socket_dir = cache.join("sockets");
    if let Ok(entries) = std::fs::read_dir(&socket_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("sock") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let lock = lock_dir.join(format!("{slug}.lock"));
            if lockfile::read(&lock).is_some_and(|l| l.is_alive()) {
                continue;
            }
            if json {
                removed_paths.push(path.display().to_string());
            } else {
                let action = if dry_run { "would remove" } else { "removing" };
                println!("{action} orphan socket: {}", path.display());
            }
            if !dry_run {
                let _ = std::fs::remove_file(&path);
            }
            removed_sockets += 1;
        }
    }

    // Orphan log files (no matching live lockfile)
    let log_dir = cache.join("logs");
    if let Ok(entries) = std::fs::read_dir(&log_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("log") {
                continue;
            }
            let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let lock = lock_dir.join(format!("{slug}.lock"));
            if lockfile::read(&lock).is_some_and(|l| l.is_alive()) {
                continue;
            }
            if json {
                removed_paths.push(path.display().to_string());
            } else {
                let action = if dry_run { "would remove" } else { "removing" };
                println!("{action} orphan log: {}", path.display());
            }
            if !dry_run {
                let _ = std::fs::remove_file(&path);
            }
            removed_logs += 1;
        }
    }

    if json {
        print_json(&CleanJson {
            removed: removed_paths,
            dry_run,
        });
    } else {
        println!(
            "{}: {} master artifact(s), {} lockfile(s), {} socket(s), {} log(s)",
            if dry_run { "Would remove" } else { "Removed" },
            removed_master,
            removed_locks,
            removed_sockets,
            removed_logs,
        );
    }
    0
}

fn clean_master_artifacts(dry_run: bool, json: bool, removed_paths: &mut Vec<String>) -> usize {
    let mut removed = 0;
    let action = if dry_run { "would remove" } else { "removing" };

    let routing = routing_table_path();
    if routing.exists() {
        if json {
            removed_paths.push(routing.display().to_string());
        } else {
            println!("{action} routing table: {}", routing.display());
        }
        if !dry_run {
            let _ = std::fs::remove_file(&routing);
        }
        removed += 1;
    }

    let master_sock = master_socket_path();
    if master_sock.exists() {
        if json {
            removed_paths.push(master_sock.display().to_string());
        } else {
            println!("{action} master socket: {}", master_sock.display());
        }
        if !dry_run {
            let _ = std::fs::remove_file(&master_sock);
        }
        removed += 1;
    }

    let master_lock = master_lockfile_path();
    if master_lock.exists() {
        if json {
            removed_paths.push(master_lock.display().to_string());
        } else {
            println!("{action} master lockfile: {}", master_lock.display());
        }
        if !dry_run {
            let _ = std::fs::remove_file(&master_lock);
        }
        removed += 1;
    }

    let workers_dir = fff_ipc::xdg_cache_dir().join("fff").join("workers");
    if let Ok(entries) = std::fs::read_dir(&workers_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
                continue;
            };
            if ext == "sock" || ext == "lock" {
                if json {
                    removed_paths.push(path.display().to_string());
                } else {
                    println!("{action} worker artifact: {}", path.display());
                }
                if !dry_run {
                    let _ = std::fs::remove_file(&path);
                }
                removed += 1;
            }
        }
    }

    removed
}

// ─────────────────────────────────────────────────────────────────────────────
// Master management helpers

/// Send one request to the master socket and return the response, or None if master is unreachable.
/// Always returns None on non-Unix platforms — the master uses Unix domain sockets.
#[cfg(unix)]
fn master_request(req: MasterRequest) -> Option<MasterResponse> {
    let socket = master_socket_path();
    let stream = UnixStream::connect(&socket).ok()?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let mut writer = BufWriter::new(stream.try_clone().ok()?);
    let mut reader = BufReader::new(stream);

    write_message_sync(&mut writer, &req).ok()?;
    use std::io::Write;
    writer.flush().ok()?;
    let resp: MasterResponse = read_message_sync(&mut reader).ok()?;
    Some(resp)
}

#[cfg(not(unix))]
fn master_request(_req: MasterRequest) -> Option<MasterResponse> {
    None
}

/// Convenience: list workers via master. Returns None if master is not running.
fn master_request_list() -> Option<Vec<fff_ipc::types::WorkerInfo>> {
    match master_request(MasterRequest::ListWorkers)? {
        MasterResponse::WorkerList { workers } => Some(workers),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers

struct Daemon {
    slug: String,
    lock: Lockfile,
    lockfile_path: PathBuf,
}

fn discover_daemons() -> Vec<Daemon> {
    let lock_dir = fff_ipc::xdg_cache_dir().join("fff").join("locks");
    let Ok(entries) = std::fs::read_dir(&lock_dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("lock") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Some(lock) = lockfile::read(&path) {
            out.push(Daemon {
                slug: slug.to_string(),
                lock,
                lockfile_path: path,
            });
        }
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

#[cfg(unix)]
fn stop_daemon(d: &Daemon, timeout: Duration) -> Result<(), String> {
    let pid = d.lock.pid as libc::pid_t;
    // SAFETY: SIGTERM to a known PID. errno on failure is surfaced via the
    // standard errno() route below.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("SIGTERM failed: {err}"));
    }

    let deadline = Instant::now() + timeout;
    let poll = Duration::from_millis(50);
    while Instant::now() < deadline {
        if !d.lock.is_alive() {
            return Ok(());
        }
        std::thread::sleep(poll);
    }

    if timeout.is_zero() {
        return Err("did not exit; --timeout 0 disables SIGKILL".into());
    }
    // SAFETY: SIGKILL after the graceful window elapsed.
    let rc = unsafe { libc::kill(pid, libc::SIGKILL) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("SIGKILL failed: {err}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn stop_daemon(_d: &Daemon, _timeout: Duration) -> Result<(), String> {
    Err("daemon stop is not supported on this platform".into())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn worker_fixture(index: u32, pid: u32, roots: Vec<(&str, &str)>) -> WorkerInfo {
        WorkerInfo {
            index,
            socket_path: format!("/tmp/fff/worker-{index}.sock"),
            roots: roots
                .into_iter()
                .map(|(slug, base)| RootEntry {
                    slug: slug.into(),
                    base_path: base.into(),
                })
                .collect(),
            pid,
        }
    }

    #[test]
    fn list_master_response_shape_is_correct() {
        let workers = vec![
            worker_fixture(0, 100, vec![("abc", "/path/a")]),
            worker_fixture(1, 101, vec![]),
        ];
        let json = ListJson::Master {
            master_pid: 12345,
            worker_count: workers.len(),
            workers: &workers,
        };
        let v: Value = serde_json::to_value(&json).unwrap();
        assert_eq!(v["mode"], "master");
        assert_eq!(v["master_pid"], 12345);
        assert_eq!(v["worker_count"], 2);
        assert_eq!(v["workers"][0]["index"], 0);
        assert_eq!(v["workers"][0]["pid"], 100);
        assert_eq!(v["workers"][0]["socket_path"], "/tmp/fff/worker-0.sock");
        assert_eq!(v["workers"][0]["roots"][0]["slug"], "abc");
        assert_eq!(v["workers"][0]["roots"][0]["base_path"], "/path/a");
        assert_eq!(v["workers"][1]["roots"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_legacy_fallback_shape_is_correct() {
        let entries = vec![
            LegacyDaemonJson {
                pid: 41046,
                state: "live",
                slug: "abc",
                base_path: Some("/path/a".into()),
            },
            LegacyDaemonJson {
                pid: 41047,
                state: "stale",
                slug: "xyz",
                base_path: None,
            },
        ];
        let v: Value = serde_json::to_value(ListJson::Legacy { daemons: entries }).unwrap();
        assert_eq!(v["mode"], "legacy");
        assert_eq!(v["daemons"][0]["pid"], 41046);
        assert_eq!(v["daemons"][0]["state"], "live");
        assert_eq!(v["daemons"][0]["slug"], "abc");
        assert_eq!(v["daemons"][0]["base_path"], "/path/a");
        assert_eq!(v["daemons"][1]["base_path"], Value::Null);
    }

    #[test]
    fn list_none_emits_empty_daemons() {
        let v: Value = serde_json::to_value(ListJson::None { daemons: [] }).unwrap();
        assert_eq!(v["mode"], "none");
        assert_eq!(v["daemons"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn paths_emits_all_expected_keys() {
        let p = PathsJson {
            base_path: "/tmp".into(),
            slug: "tmp-abc".into(),
            socket: "/c/sockets/tmp-abc.sock".into(),
            lockfile: "/c/locks/tmp-abc.lock".into(),
            frecency: "/d/frecency/tmp-abc".into(),
            log: "/c/logs/tmp-abc.log".into(),
            master_sock: "/c/master.sock".into(),
            master_lock: "/c/master.lock".into(),
            routing: "/c/routing.json".into(),
        };
        let v: Value = serde_json::to_value(&p).unwrap();
        for key in [
            "base_path",
            "slug",
            "socket",
            "lockfile",
            "frecency",
            "log",
            "master_sock",
            "master_lock",
            "routing",
        ] {
            assert!(v.get(key).is_some(), "missing key: {key}");
            assert!(v[key].is_string(), "key {key} should be a string");
        }
    }

    #[test]
    fn error_json_shape_is_correct() {
        let v: Value = serde_json::to_value(ErrorJson {
            error: "master not running",
        })
        .unwrap();
        assert_eq!(v, json!({"error": "master not running"}));
    }

    #[test]
    fn clean_json_omits_dry_run_when_false() {
        let s = serde_json::to_string(&CleanJson {
            removed: vec!["/a".into(), "/b".into()],
            dry_run: false,
        })
        .unwrap();
        assert_eq!(s, r#"{"removed":["/a","/b"]}"#);

        let s2 = serde_json::to_string(&CleanJson {
            removed: vec![],
            dry_run: true,
        })
        .unwrap();
        assert_eq!(s2, r#"{"removed":[],"dry_run":true}"#);
    }

    #[test]
    fn stop_ok_json_shape() {
        let v: Value = serde_json::to_value(StopOkJson {
            ok: true,
            stopped: 3,
        })
        .unwrap();
        assert_eq!(v, json!({"ok": true, "stopped": 3}));
    }

    #[test]
    fn json_output_is_single_line_compact() {
        let workers = vec![worker_fixture(0, 1, vec![("a", "/a")])];
        let s = serde_json::to_string(&ListJson::Master {
            master_pid: 1,
            worker_count: 1,
            workers: &workers,
        })
        .unwrap();
        assert!(!s.contains('\n'), "JSON output must be single-line");
        assert!(!s.contains("  "), "JSON output must be compact");
    }
}
