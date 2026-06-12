use fff_ipc::config::FffConfig;

#[cfg(unix)]
use std::{
    collections::HashMap,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use fff_ipc::protocol::{
    BasePathParams, FindFilesParams, GetGitStatusParams, GrepParams, ListDirectoriesParams,
    ListRecentFilesParams, MultiGrepParams, RecordAccessParams, RequestEnvelope, ResponseEnvelope,
    ResponseError, check_protocol_version, verbs,
};
#[cfg(unix)]
use fff_ipc::{
    BAD_REQUEST, decode_bincode, decode_json, looks_like_json, read_frame, read_message,
    write_json_message, write_message,
};
#[cfg(unix)]
use fff_ipc::{
    base_path_slug, master_lockfile_path, master_socket_path,
    types::{HealthResponse, MasterRequest, RootHealth, SearchRequest, SearchResponse},
    worker_lockfile_path, worker_socket_path, write_message_sync,
};
#[cfg(unix)]
use parking_lot::{Mutex, RwLock};
#[cfg(unix)]
use std::time::{Duration, Instant};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(unix)]
use tokio::sync::Mutex as TokioMutex;

#[cfg(unix)]
use crate::state::{EffectiveArgs, EngineState};

#[cfg(unix)]
struct RootEntry {
    state: Arc<EngineState>,
    // Canonicalized base_path, precomputed once so ancestor matching in
    // get_or_init stays I/O-free under the read lock.
    canonical_base: PathBuf,
    // Milliseconds since Unix epoch; updated atomically on every access.
    // Allows fast-path reads to hold roots.read() instead of roots.write().
    last_access_ms: AtomicU64,
    // Set once when the root finishes its initial scan. Acts as the
    // last-full-scan timestamp surfaced via `fffctl health`.
    loaded_at: Instant,
}

#[cfg(unix)]
pub(crate) struct WorkerState {
    pub index: u32,
    config: FffConfig,
    roots: Arc<RwLock<HashMap<String, RootEntry>>>,
    // Per-slug async mutex serialises concurrent inits for the same root.
    // Outer Mutex is sync (held only briefly to clone the inner Arc).
    init_gates: Mutex<HashMap<String, Arc<TokioMutex<()>>>>,
}

#[cfg(unix)]
impl WorkerState {
    pub fn new(index: u32, config: FffConfig) -> Self {
        Self {
            index,
            config,
            roots: Arc::new(RwLock::new(HashMap::new())),
            init_gates: Mutex::new(HashMap::new()),
        }
    }

    // Return a loaded `Arc<EngineState>` for `base_path`, initialising it on first access.
    // Two concurrent callers for the same slug serialise behind the slug's gate;
    // the second caller hits the registry after the first completes init.
    pub async fn get_or_init(&self, base_path: PathBuf) -> Result<Arc<EngineState>, String> {
        let slug = base_path_slug(&base_path);
        let max_roots = self.config.worker.roots_per_worker_max as usize;
        let now = now_ms();

        // Canonicalize once (I/O kept outside the lock) for ancestor matching.
        let canonical_req = std::fs::canonicalize(&base_path).unwrap_or_else(|_| base_path.clone());

        // Fast path: exact slug, or an already-loaded ancestor root (containment).
        // The master routes a sub-path Handshake to the containing root's worker;
        // here we bind the Connect to that root's EngineState instead of minting
        // a duplicate index for the sub-path.
        if let Some(state) = self.resolve_loaded(&slug, &canonical_req, now) {
            return Ok(state);
        }

        // Slow path: gate serialises concurrent inits for the same slug.
        let gate = {
            let mut gates = self.init_gates.lock();
            Arc::clone(
                gates
                    .entry(slug.clone())
                    .or_insert_with(|| Arc::new(TokioMutex::new(()))),
            )
        };
        let _gate_guard = gate.lock().await;

        // Double-check after acquiring gate.
        if let Some(state) = self.resolve_loaded(&slug, &canonical_req, now) {
            return Ok(state);
        }

        if self.roots.read().len() >= max_roots {
            self.evict_lru().await;
        }

        let args = EffectiveArgs {
            base_path: base_path.clone(),
            frecency_db_path: self.config.frecency.db.as_deref().map(PathBuf::from),
            no_watch: self.config.index.no_watch,
            no_warmup: self.config.index.no_warmup,
            ignore: self.config.mcp.ignore_for(&base_path),
        };

        // Convert error to String inside closure so the return type is Send.
        let new_state = tokio::task::spawn_blocking(move || {
            crate::state::init(&args).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| format!("spawn_blocking join error: {e}"))??;

        let new_state = Arc::new(new_state);
        let canonical_base = std::fs::canonicalize(&new_state.base_path)
            .unwrap_or_else(|_| new_state.base_path.clone());
        self.roots.write().insert(
            slug,
            RootEntry {
                state: Arc::clone(&new_state),
                canonical_base,
                last_access_ms: AtomicU64::new(now_ms()),
                loaded_at: Instant::now(),
            },
        );

        Ok(new_state)
    }

    // Resolve a request to an already-loaded root: exact slug first, then the
    // longest-prefix ancestor (containment). Updates last_access on a hit.
    // Held entirely under one read lock — no I/O (canonical paths precomputed).
    fn resolve_loaded(
        &self,
        slug: &str,
        canonical_req: &Path,
        now: u64,
    ) -> Option<Arc<EngineState>> {
        let map = self.roots.read();
        if let Some(entry) = map.get(slug) {
            entry.last_access_ms.store(now, Ordering::Relaxed);
            return Some(Arc::clone(&entry.state));
        }
        let mut best: Option<&RootEntry> = None;
        let mut best_len = 0usize;
        for entry in map.values() {
            let len = entry.canonical_base.as_os_str().len();
            if canonical_req.starts_with(&entry.canonical_base) && len > best_len {
                best_len = len;
                best = Some(entry);
            }
        }
        best.map(|entry| {
            entry.last_access_ms.store(now, Ordering::Relaxed);
            Arc::clone(&entry.state)
        })
    }

    // Subsume a child root: merge its frecency into the parent when both are
    // co-resident on this worker, then remove the child's EngineState. Sent by
    // the master during containment subsumption. Arcs are cloned under a brief
    // read lock; the merge runs lock-free; removal takes the write lock.
    fn drop_root(&self, slug: &str, merge_into_slug: &str) {
        let (child, parent) = {
            let map = self.roots.read();
            (
                map.get(slug).map(|e| Arc::clone(&e.state)),
                map.get(merge_into_slug).map(|e| Arc::clone(&e.state)),
            )
        };
        match (&child, &parent) {
            (Some(child), Some(parent)) => merge_child_frecency(parent, child),
            (Some(_), None) => tracing::warn!(
                "worker-{}: subsuming {slug} but parent {merge_into_slug} is not loaded here \
                 — frecency not merged (parent re-accrues signal as it is used)",
                self.index
            ),
            _ => {}
        }
        if self.roots.write().remove(slug).is_some() {
            tracing::info!("worker-{}: dropped subsumed root {slug}", self.index);
        }
    }

    // Snapshot freshness signals for every loaded root.
    // Read lock is held only long enough to clone the per-root metadata —
    // the actual file count / dirty count read happens after dropping the
    // worker-level lock by going through each EngineState's picker.
    fn collect_health(&self) -> HealthResponse {
        let snapshot: Vec<(String, Arc<EngineState>, Instant)> = {
            let map = self.roots.read();
            map.iter()
                .map(|(slug, entry)| (slug.clone(), Arc::clone(&entry.state), entry.loaded_at))
                .collect()
        };

        let now = Instant::now();
        let roots = snapshot
            .into_iter()
            .map(|(slug, state, loaded_at)| {
                let (indexed_files, dirty_count) = read_picker_freshness(&state);
                RootHealth {
                    slug,
                    base_path: state.base_path.to_string_lossy().into_owned(),
                    indexed_files,
                    last_scan_age_sec: Some(now.duration_since(loaded_at).as_secs()),
                    watcher_backlog: None,
                    dirty_count,
                }
            })
            .collect();

        HealthResponse { roots }
    }

    // Evict the LRU root with no active connections (Arc::strong_count == 1).
    // Roots with live connections are skipped; if none are evictable the new
    // root loads anyway as a temporary overflow.
    async fn evict_lru(&self) {
        let victim = {
            let map = self.roots.read();
            map.iter()
                .filter(|(_, e)| Arc::strong_count(&e.state) == 1)
                .min_by_key(|(_, e)| e.last_access_ms.load(Ordering::Relaxed))
                .map(|(slug, _)| slug.clone())
        };

        if let Some(slug) = victim {
            self.roots.write().remove(&slug);
            tracing::debug!("worker-{}: evicted root {slug}", self.index);
            self.notify_evicted(slug).await;
        }
    }

    // Fire-and-forget EvictedRoot to master socket.
    // Uses spawn_blocking because std::os::unix::net::UnixStream::connect is blocking.
    // Failure is benign — idle TTL will clean up the routing entry.
    #[cfg(unix)]
    async fn notify_evicted(&self, slug: String) {
        let master = master_socket_path();
        let msg = MasterRequest::EvictedRoot { slug };
        tokio::task::spawn_blocking(move || {
            use std::net::Shutdown;
            use std::os::unix::net::UnixStream;
            if let Ok(mut stream) = UnixStream::connect(&master) {
                let _ = write_message_sync(&mut stream, &msg);
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
    }

    #[cfg(not(unix))]
    async fn notify_evicted(&self, _slug: String) {}
}

// Read indexed_files and dirty_count off the shared picker without blocking
// for long: returns (None, None) when the picker is mid-init or contended.
#[cfg(unix)]
fn read_picker_freshness(state: &EngineState) -> (Option<u64>, Option<u64>) {
    let Ok(guard) = state.shared_picker.read() else {
        return (None, None);
    };
    let Some(picker) = guard.as_ref() else {
        return (None, None);
    };
    let indexed = picker.live_file_count() as u64;
    let dirty = picker
        .get_files()
        .iter()
        .filter(|f| !f.is_deleted() && f.git_status.is_some_and(fff::git::is_modified_status))
        .count() as u64;
    (Some(indexed), Some(dirty))
}

// Orphan self-heal cadence. GRACE must comfortably exceed a master restart so a
// briefly-absent master (re-adopting via routing.json) does not trigger exit.
// Both are overridable via env for tests (defaults used in production).
#[cfg(unix)]
const ORPHAN_CHECK_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(unix)]
const ORPHAN_GRACE: Duration = Duration::from_secs(60);

#[cfg(unix)]
fn env_duration_secs(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()?
        .parse()
        .ok()
        .map(Duration::from_secs)
}

/// Entry point for worker mode. Binds the worker socket and serves connections.
#[cfg(unix)]
pub async fn run(index: u32, config: FffConfig) -> Result<(), Box<dyn std::error::Error>> {
    let socket_path = worker_socket_path(index);
    let lockfile_path = worker_lockfile_path(index);

    if let Some(parent) = lockfile_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Use O_CREAT|O_EXCL so two concurrent workers with the same index cannot
    // both overwrite each other's PID (unlike plain std::fs::write).
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lockfile_path)
    {
        Ok(_) => {
            std::fs::write(&lockfile_path, format!("{}\n", std::process::id()))?;
        }
        Err(_) => {
            if fff_ipc::lockfile::is_stale(&lockfile_path) {
                let _ = std::fs::remove_file(&lockfile_path);
                std::fs::write(&lockfile_path, format!("{}\n", std::process::id()))?;
            } else {
                tracing::info!("worker-{index}: another instance already running, exiting");
                return Ok(());
            }
        }
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(
        "fff-engine worker-{index} listening on {}",
        socket_path.display()
    );

    let worker_state = Arc::new(WorkerState::new(index, config));

    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("worker-{index} SIGINT"),
            _ = sigterm.recv() => tracing::info!("worker-{index} SIGTERM"),
        }
    };
    tokio::pin!(shutdown);

    // Self-heal against orphaning. The master reaps workers via SIGTERM only on
    // its own graceful shutdown; a SIGKILL/crash/OOM (or a test dropping it)
    // leaves us running with no one to stop us. Watch the master lockfile and,
    // once the watch is armed and the master has been gone for ORPHAN_GRACE,
    // exit. Arming requires either having seen a live master (the normal case)
    // or being reparented to PID 1 (our master died before we first observed
    // it). A restarted master rewrites the lockfile (and re-adopts us via
    // routing.json) well within the grace window, so normal restarts don't trip
    // this, and a standalone/test worker with a living non-master parent and no
    // master lockfile never arms.
    let orphan_check = env_duration_secs("FFF_ORPHAN_CHECK_SECS").unwrap_or(ORPHAN_CHECK_INTERVAL);
    let orphan_grace = env_duration_secs("FFF_ORPHAN_GRACE_SECS").unwrap_or(ORPHAN_GRACE);
    let orphan_watch = async move {
        let master_lock = master_lockfile_path();
        let mut seen_master = false;
        let mut dead_since: Option<Instant> = None;
        loop {
            tokio::time::sleep(orphan_check).await;
            if fff_ipc::lockfile::is_stale(&master_lock) {
                let orphaned = unsafe { libc::getppid() } == 1;
                if seen_master || orphaned {
                    let since = *dead_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= orphan_grace {
                        return;
                    }
                }
            } else {
                seen_master = true;
                dead_since = None;
            }
        }
    };
    tokio::pin!(orphan_watch);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let ws = Arc::clone(&worker_state);
                        tokio::spawn(handle_worker_connection(stream, ws));
                    }
                    Err(e) => tracing::error!("worker-{index} accept error: {e}"),
                }
            }
            _ = &mut shutdown => break,
            _ = &mut orphan_watch => {
                tracing::warn!(
                    "worker-{index}: no live master for {orphan_grace:?}; exiting to avoid orphaning"
                );
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&lockfile_path);
    tracing::info!("fff-engine worker-{index} stopped");
    Ok(())
}

#[cfg(not(unix))]
pub async fn run(_index: u32, _config: FffConfig) -> Result<(), Box<dyn std::error::Error>> {
    Err("fff-engine worker mode is not supported on this platform".into())
}

#[cfg(unix)]
async fn handle_worker_connection(stream: tokio::net::UnixStream, ws: Arc<WorkerState>) {
    let (mut read_half, write_half) = tokio::io::split(stream);

    // Dual-read: sniff the first frame's leading byte. `{` ⇒ versioned JSON
    // envelope; anything else ⇒ legacy bincode SearchRequest. The session stays
    // in whichever mode it opened — response encoding mirrors request encoding.
    let first = match read_frame(&mut read_half).await {
        Ok(bytes) => bytes,
        Err(_) => return,
    };

    if looks_like_json(&first) {
        handle_json_connection(first, read_half, write_half, ws).await;
    } else {
        handle_bincode_connection(first, read_half, write_half, ws).await;
    }
}

// Legacy bincode path — byte-for-byte behaviorally identical to the pre-dual-read
// worker (R5). The first frame's bytes are already read; decode them as a
// SearchRequest and proceed exactly as before.
#[cfg(unix)]
async fn handle_bincode_connection(
    first: Vec<u8>,
    mut read_half: tokio::io::ReadHalf<tokio::net::UnixStream>,
    mut write_half: tokio::io::WriteHalf<tokio::net::UnixStream>,
    ws: Arc<WorkerState>,
) {
    let base_path = match decode_bincode::<SearchRequest>(&first) {
        Ok(SearchRequest::Connect { base_path }) => PathBuf::from(base_path),
        Ok(SearchRequest::Health) => {
            let health = ws.collect_health();
            let _ = write_message(&mut write_half, &SearchResponse::Health(health)).await;
            return;
        }
        Ok(SearchRequest::DropRoot {
            slug,
            merge_into_slug,
        }) => {
            ws.drop_root(&slug, &merge_into_slug);
            let _ = write_message(&mut write_half, &SearchResponse::Ack).await;
            return;
        }
        Ok(other) => {
            tracing::warn!(
                "worker-{}: unexpected first message {:?}, closing",
                ws.index,
                other
            );
            return;
        }
        Err(_) => return,
    };

    let state = match ws.get_or_init(base_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("worker-{}: state init failed: {e}", ws.index);
            let _ = write_message(&mut write_half, &SearchResponse::Error(e)).await;
            return;
        }
    };

    if write_message(&mut write_half, &SearchResponse::Ack)
        .await
        .is_err()
    {
        return;
    }

    loop {
        let req: SearchRequest = match read_message(&mut read_half).await {
            Ok(r) => r,
            Err(_) => break,
        };

        match req {
            SearchRequest::Connect { .. } => {
                let _ = write_message(
                    &mut write_half,
                    &SearchResponse::Error("unexpected Connect after handshake".into()),
                )
                .await;
                break;
            }
            SearchRequest::DropRoot { .. } => {
                // DropRoot is only valid as a first message on a fresh
                // connection; reject mid-session rather than reaching the
                // dispatch_request unreachable.
                let _ = write_message(
                    &mut write_half,
                    &SearchResponse::Error("unexpected DropRoot mid-session".into()),
                )
                .await;
                break;
            }
            SearchRequest::RecordAccess { path } => {
                record_access(&state, path);
            }
            SearchRequest::SetLogLevel { level } => {
                let response = match crate::set_log_level(&level) {
                    Ok(()) => SearchResponse::Ack,
                    Err(e) => SearchResponse::Error(e),
                };
                if write_message(&mut write_half, &response).await.is_err() {
                    break;
                }
            }
            req => {
                let response = crate::server::dispatch_request(&state, req).await;
                if write_message(&mut write_half, &response).await.is_err() {
                    break;
                }
            }
        }
    }
}

// Versioned JSON path. First message must be `connect` (version-checked, then
// loads the root and acks) or `health` (one-shot, then close); any other first
// verb is rejected, mirroring the legacy "first message must be Connect"
// discipline. A connection that opened JSON stays JSON for its lifetime.
#[cfg(unix)]
async fn handle_json_connection(
    first: Vec<u8>,
    mut read_half: tokio::io::ReadHalf<tokio::net::UnixStream>,
    mut write_half: tokio::io::WriteHalf<tokio::net::UnixStream>,
    ws: Arc<WorkerState>,
) {
    let env: RequestEnvelope = match decode_json(&first) {
        Ok(e) => e,
        Err(_) => {
            let _ =
                write_json_message(&mut write_half, &bad_request("malformed JSON envelope")).await;
            return;
        }
    };

    if let Err(mismatch) = check_protocol_version(env.protocol_version) {
        let _ = write_json_message(&mut write_half, &mismatch).await;
        return;
    }

    let state = match env.verb.as_str() {
        verbs::CONNECT => {
            let base_path = match env.params_as::<BasePathParams>() {
                Ok(p) => PathBuf::from(p.base_path),
                Err(err_env) => {
                    let _ = write_json_message(&mut write_half, &err_env).await;
                    return;
                }
            };
            match ws.get_or_init(base_path).await {
                Ok(s) => {
                    let ack = ResponseEnvelope::ok(&serde_json::json!({ "ack": true }));
                    match ack {
                        Ok(env) if write_json_message(&mut write_half, &env).await.is_ok() => s,
                        _ => return,
                    }
                }
                Err(e) => {
                    tracing::error!("worker-{}: state init failed: {e}", ws.index);
                    let _ = write_json_message(
                        &mut write_half,
                        &ResponseEnvelope::err(ResponseError {
                            code: fff_ipc::INTERNAL.to_string(),
                            message: e,
                            engine_version: None,
                            client_version: None,
                        }),
                    )
                    .await;
                    return;
                }
            }
        }
        verbs::HEALTH => {
            let health = ws.collect_health();
            if let Ok(env) = ResponseEnvelope::ok(&health) {
                let _ = write_json_message(&mut write_half, &env).await;
            }
            return;
        }
        _ => {
            let _ = write_json_message(
                &mut write_half,
                &bad_request("first JSON message must be 'connect' or 'health'"),
            )
            .await;
            return;
        }
    };

    json_request_loop(&state, &mut read_half, &mut write_half).await;
}

// Per-request JSON loop: decode each frame as a RequestEnvelope, map the verb to
// the existing SearchRequest, reuse `dispatch_request`, then serialize the
// returned SearchResponse as the JSON `result`. `record_access` is
// fire-and-forget — it writes no response frame (parity with bincode).
#[cfg(unix)]
async fn json_request_loop(
    state: &EngineState,
    read_half: &mut tokio::io::ReadHalf<tokio::net::UnixStream>,
    write_half: &mut tokio::io::WriteHalf<tokio::net::UnixStream>,
) {
    loop {
        let frame = match read_frame(read_half).await {
            Ok(f) => f,
            Err(_) => break,
        };
        let env: RequestEnvelope = match decode_json(&frame) {
            Ok(e) => e,
            Err(_) => {
                let _ =
                    write_json_message(write_half, &bad_request("malformed JSON envelope")).await;
                break;
            }
        };

        // record_access is fire-and-forget: perform the write, send no frame.
        // Malformed params are logged and dropped — never reply (the client
        // awaits nothing) and never tear down the session over a no-reply verb.
        if env.verb == verbs::RECORD_ACCESS {
            match env.params_as::<RecordAccessParams>() {
                Ok(p) => record_access(state, p.path),
                Err(_) => {
                    tracing::warn!("worker: dropping record_access with malformed params");
                }
            }
            continue;
        }

        let response = match envelope_to_request(&env) {
            Ok(req) => {
                let resp = crate::server::dispatch_request(state, req).await;
                match crate::server::searchresponse_to_json_value(resp) {
                    Ok(value) => match ResponseEnvelope::ok(&value) {
                        Ok(env) => env,
                        Err(_) => break,
                    },
                    Err(err) => ResponseEnvelope::err(err),
                }
            }
            Err(err_env) => err_env,
        };

        if write_json_message(write_half, &response).await.is_err() {
            break;
        }
    }
}

// Map a JSON request envelope to the existing SearchRequest. Returns a
// BAD_REQUEST error envelope for unknown verbs or undeserializable params.
#[cfg(unix)]
fn envelope_to_request(env: &RequestEnvelope) -> Result<SearchRequest, ResponseEnvelope> {
    let req = match env.verb.as_str() {
        verbs::GREP => {
            let p: GrepParams = env.params_as()?;
            SearchRequest::Grep {
                query: p.query,
                options: p.options,
            }
        }
        verbs::FIND_FILES => {
            let p: FindFilesParams = env.params_as()?;
            SearchRequest::FindFiles {
                query: p.query,
                options: p.options,
            }
        }
        verbs::MULTI_GREP => {
            let p: MultiGrepParams = env.params_as()?;
            SearchRequest::MultiGrep {
                patterns: p.patterns,
                constraints: p.constraints,
                options: p.options,
            }
        }
        verbs::LIST_RECENT_FILES => {
            let p: ListRecentFilesParams = env.params_as()?;
            SearchRequest::ListRecentFiles {
                limit: p.limit,
                dirty_only: p.dirty_only,
            }
        }
        verbs::GET_GIT_STATUS => {
            let p: GetGitStatusParams = env.params_as()?;
            SearchRequest::GetGitStatus {
                include_clean: p.include_clean,
            }
        }
        verbs::LIST_DIRECTORIES => {
            let p: ListDirectoriesParams = env.params_as()?;
            SearchRequest::ListDirectories { limit: p.limit }
        }
        other => {
            return Err(bad_request(&format!("unknown verb '{other}'")));
        }
    };
    Ok(req)
}

#[cfg(unix)]
fn bad_request(message: &str) -> ResponseEnvelope {
    ResponseEnvelope::err(ResponseError {
        code: BAD_REQUEST.to_string(),
        message: message.to_string(),
        engine_version: None,
        client_version: None,
    })
}

// Shared fire-and-forget frecency write for both encodings.
#[cfg(unix)]
fn record_access(state: &EngineState, path: String) {
    let frecency = state.shared_frecency.clone();
    let base = state.base_path.clone();
    tokio::task::spawn_blocking(move || {
        let abs_path = if std::path::Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            base.join(&path)
        };
        if let Ok(guard) = frecency.read()
            && let Some(tracker) = guard.as_ref()
            && let Err(e) = tracker.track_access(&abs_path)
        {
            tracing::warn!(?abs_path, "RecordAccess failed: {e}");
        }
    });
}

// Merge a subsumed child's frecency history into the parent's DB, in-process,
// using the already-open trackers (no cross-process LMDB env aliasing). Keys
// are absolute-path hashes, identical across roots, so the union is direct.
#[cfg(unix)]
fn merge_child_frecency(parent: &EngineState, child: &EngineState) {
    if let (Ok(pg), Ok(cg)) = (parent.shared_frecency.read(), child.shared_frecency.read())
        && let (Some(p), Some(c)) = (pg.as_ref(), cg.as_ref())
    {
        match p.merge_from(c) {
            Ok(n) => tracing::info!(merged = n, "subsumption: merged child frecency into parent"),
            Err(e) => tracing::warn!("subsumption: frecency merge failed: {e}"),
        }
    }
}

#[cfg(unix)]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
