#[cfg(unix)]
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(unix)]
use fff_ipc::{
    BAD_REQUEST, BasePathParams, HandshakeResult, INTERNAL, RequestEnvelope, ResponseEnvelope,
    ResponseError, base_path_slug, check_protocol_version,
    config::WorkerConfig,
    decode_bincode, decode_json, looks_like_json, master_lockfile_path, master_socket_path,
    protocol::{WireRoot, verbs},
    read_frame, read_message,
    routing::{RoutingTable, WorkerEntry},
    routing_table_path,
    types::{
        HealthReport, MasterRequest, MasterResponse, SearchRequest, SearchResponse, WorkerHealth,
        WorkerInfo,
    },
    worker_socket_path, write_json_message, write_message,
};
#[cfg(unix)]
use tokio::{
    net::UnixListener,
    process::Command,
    sync::Mutex,
    time::{interval, sleep},
};

#[cfg(unix)]
use crate::ring::HashRing;

#[cfg(unix)]
struct MasterState {
    config: WorkerConfig,
    exe_path: PathBuf,
    routing: Mutex<RoutingTable>,
    ring: Mutex<HashRing>,
    /// Single-flight latch for the empty-ring bootstrap spawn (see assign_new_root).
    bootstrap: Mutex<()>,
    /// Workers spawned this session (have Child handles for try_wait monitoring).
    children: Mutex<HashMap<u32, tokio::process::Child>>,
    /// PIDs of workers adopted from routing.json (master restart — no Child handle).
    adopted_pids: Mutex<HashMap<u32, u32>>,
    /// Monotonically increasing worker index counter.
    next_index: Mutex<u32>,
    /// When each worker's routing table last became empty (for idle TTL).
    idle_since: Mutex<HashMap<u32, Instant>>,
    /// Consecutive routing.json save failure count — resets on success.
    save_fail_count: AtomicU32,
    /// Master startup time — used to report uptime via `fffctl health`.
    started_at: Instant,
    /// Configured `[mcp]` roots (name, canonical path, is_default), default-first.
    /// Source of name/default for the `list_roots` verb.
    configured_roots: Vec<ConfiguredRoot>,
    /// Slugs of configured roots — exempt from idle/path-gone eviction.
    /// Precomputed from `configured_roots` so a later path-gone canonicalization
    /// can't shift the slug and un-exempt a configured root.
    configured_slugs: HashSet<String>,
}

// A configured `[mcp]` root, canonicalized and tagged with its default flag.
#[cfg(unix)]
struct ConfiguredRoot {
    name: Option<String>,
    path: PathBuf,
    default: bool,
}

#[cfg(unix)]
impl MasterState {
    fn new(
        config: WorkerConfig,
        exe_path: PathBuf,
        routing: RoutingTable,
        ring: HashRing,
        next_index: u32,
        adopted_pids: HashMap<u32, u32>,
        configured_roots: Vec<ConfiguredRoot>,
    ) -> Self {
        let configured_slugs = configured_roots
            .iter()
            .map(|c| base_path_slug(&c.path))
            .collect();
        Self {
            config,
            exe_path,
            routing: Mutex::new(routing),
            ring: Mutex::new(ring),
            bootstrap: Mutex::new(()),
            children: Mutex::new(HashMap::new()),
            adopted_pids: Mutex::new(adopted_pids),
            next_index: Mutex::new(next_index),
            idle_since: Mutex::new(HashMap::new()),
            save_fail_count: AtomicU32::new(0),
            started_at: Instant::now(),
            configured_roots,
            configured_slugs,
        }
    }

    /// Persist the routing table, logging escalating warnings on repeated failures.
    fn persist_routing(&self, routing: &RoutingTable) {
        match routing.save(&routing_table_path()) {
            Ok(()) => {
                self.save_fail_count.store(0, Ordering::Relaxed);
            }
            Err(e) => {
                let n = self.save_fail_count.fetch_add(1, Ordering::Relaxed) + 1;
                if n >= 3 {
                    tracing::error!(
                        "master: routing.json persist failed {n} consecutive times \
                         (disk full or permissions error?): {e}"
                    );
                } else {
                    tracing::warn!("master: routing.json persist failed ({n}/3): {e}");
                }
            }
        }
    }

    async fn alloc_index(&self) -> u32 {
        let mut idx = self.next_index.lock().await;
        let i = *idx;
        *idx += 1;
        i
    }

    // Spawn workers until at least `target` exist, single-flighted via the
    // bootstrap latch so the startup n_min fill and the on-demand Handshake
    // bootstrap can't both spawn for the same empty ring.
    async fn ensure_min_workers(&self, target: u32) {
        let _bootstrap = self.bootstrap.lock().await;
        loop {
            let have = self.routing.lock().await.workers.len() as u32;
            if have >= target {
                break;
            }
            let index = self.alloc_index().await;
            if let Err(e) = self.spawn_worker(index).await {
                tracing::error!("master: initial spawn failed: {e}");
                break;
            }
        }
    }

    // Spawn a new worker process and register it in the ring and routing table.
    async fn spawn_worker(&self, index: u32) -> Result<(), String> {
        let socket = worker_socket_path(index);
        let child = Command::new(&self.exe_path)
            .args(["--worker-index", &index.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn worker-{index}: {e}"))?;

        let pid = child.id().unwrap_or(0);

        // Poll until worker socket accepts connections (not just file existence).
        let sock = socket.clone();
        tokio::task::spawn_blocking(move || {
            fff_ipc::wait_for_socket(&sock, Duration::from_secs(10))
        })
        .await
        .map_err(|e| format!("join error: {e}"))?
        .map_err(|e| format!("worker-{index} socket timeout: {e}"))?;

        // Update ring (lock then release before locking routing).
        let ring_snapshot = {
            let mut ring = self.ring.lock().await;
            ring.add_worker_default(index);
            ring.to_serializable()
        };

        // Update routing table and persist.
        {
            let mut routing = self.routing.lock().await;
            routing.workers.insert(
                index,
                WorkerEntry::new(index, socket.to_string_lossy().into(), pid),
            );
            routing.ring_state = ring_snapshot;
            self.persist_routing(&routing);
        }

        self.children.lock().await.insert(index, child);

        tracing::info!("master: spawned worker-{index} pid={pid}");
        Ok(())
    }

    async fn collect_worker_info(&self) -> Vec<WorkerInfo> {
        let routing = self.routing.lock().await;
        routing
            .workers
            .values()
            .map(|e| WorkerInfo {
                index: e.index,
                socket_path: e.socket_path.clone(),
                roots: e.roots.clone(),
                pid: e.pid,
            })
            .collect()
    }

    // Fan out a Health request to every live worker and aggregate the
    // per-root snapshots. A worker that fails to respond is reported with an
    // empty roots vec — partial telemetry is still useful for AI consumers.
    async fn collect_health(&self) -> HealthReport {
        let targets: Vec<(u32, u32, std::path::PathBuf)> = {
            let routing = self.routing.lock().await;
            routing
                .workers
                .values()
                .map(|e| (e.index, e.pid, worker_socket_path(e.index)))
                .collect()
        };

        let mut handles = Vec::with_capacity(targets.len());
        for (index, pid, sock) in targets {
            handles.push(tokio::spawn(async move {
                let roots = match query_worker_health(&sock).await {
                    Ok(resp) => resp.roots,
                    Err(e) => {
                        tracing::warn!("master: worker-{index} health query failed: {e}");
                        Vec::new()
                    }
                };
                WorkerHealth {
                    index,
                    pid,
                    socket_path: sock.to_string_lossy().into_owned(),
                    roots,
                }
            }));
        }

        let mut workers = Vec::with_capacity(handles.len());
        for h in handles {
            if let Ok(w) = h.await {
                workers.push(w);
            }
        }
        workers.sort_by_key(|w| w.index);

        HealthReport {
            master_pid: std::process::id(),
            uptime_sec: self.started_at.elapsed().as_secs(),
            workers,
        }
    }

    // Resolve a base_path to its worker socket + index. Routing-table hit
    // returns immediately; a miss assigns a new root (may scale out). Shared by
    // the bincode `Handshake` arm and the JSON `handshake` verb so both paths
    // run identical routing logic.
    async fn resolve_worker_socket(
        self: &Arc<Self>,
        base_path: &str,
    ) -> Result<(String, u32), String> {
        let slug = base_path_slug(std::path::Path::new(base_path));

        let routing_hit = {
            let routing = self.routing.lock().await;
            routing.workers.iter().find_map(|(&idx, e)| {
                if e.contains_slug(&slug) {
                    Some(idx)
                } else {
                    None
                }
            })
        };

        if let Some(index) = routing_hit {
            let socket = worker_socket_path(index).to_string_lossy().into_owned();
            return Ok((socket, index));
        }

        match self.assign_new_root(base_path).await {
            Some(index) => {
                let socket = worker_socket_path(index).to_string_lossy().into_owned();
                Ok((socket, index))
            }
            None => Err("no workers available".into()),
        }
    }

    async fn worker_info(&self, index: u32) -> Option<WorkerInfo> {
        let routing = self.routing.lock().await;
        routing.workers.get(&index).map(|e| WorkerInfo {
            index: e.index,
            socket_path: e.socket_path.clone(),
            roots: e.roots.clone(),
            pid: e.pid,
        })
    }

    // Send SIGTERM (then SIGKILL after 5s if needed) to a worker and remove it from state.
    async fn stop_worker(&self, index: u32) {
        let child = self.children.lock().await.remove(&index);
        if let Some(c) = child {
            // Get PID before consuming the child, then send SIGTERM for graceful shutdown.
            if let Some(pid) = c.id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                // Give the worker up to 5s to exit cleanly before forcing SIGKILL.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while tokio::time::Instant::now() < deadline {
                    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                        break; // process gone
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if unsafe { libc::kill(pid as libc::pid_t, 0) == 0 } {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        } else if let Some(&pid) = self.adopted_pids.lock().await.get(&index) {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        self.adopted_pids.lock().await.remove(&index);

        // Remove from ring and routing, then persist.
        {
            let mut ring = self.ring.lock().await;
            ring.remove_worker(index);
            let ring_snapshot = ring.to_serializable();
            let mut routing = self.routing.lock().await;
            routing.workers.remove(&index);
            routing.ring_state = ring_snapshot;
            self.persist_routing(&routing);
        }

        tracing::info!("master: stopped worker-{index}");
    }

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

    // Called after Handshake when a slug has no routing entry (new root).
    // Ring assignment is read first (deterministic, no mutation), then the
    // routing write-lock covers presence-check + push + scale-out threshold
    // atomically — eliminating the concurrent-Handshake double-push race.
    async fn assign_new_root(self: &Arc<Self>, base_path: &str) -> Option<u32> {
        let slug = base_path_slug(std::path::Path::new(base_path));

        // Canonicalize once up front (I/O kept outside locks). Used for the
        // containment check and stored as the RootEntry.base_path.
        let canonical = std::fs::canonicalize(base_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| base_path.to_string());

        // Git working-tree root of the candidate, resolved outside any lock
        // (filesystem I/O). A linked worktree / submodule / nested repo
        // resolves to its own workdir, distinct from a lexical ancestor.
        let candidate_wt = fff::git::working_tree_root(std::path::Path::new(&canonical));

        // Containment: if an already-registered root is an ancestor of this
        // path, route to its worker instead of minting a new slug/index/
        // frecency DB/watcher. Checked before ring assignment so a contained
        // sub-path never triggers a worker spawn. Only honored when the
        // ancestor shares the candidate's git working tree — a path that is
        // its own working tree (e.g. an agent's worktree under
        // .claude/worktrees/) is a distinct, possibly-drifted checkout and
        // must get its own index, not the ancestor's view.
        let contained = {
            let routing = self.routing.lock().await;
            routing.containing_root(std::path::Path::new(&canonical))
        };
        if let Some((idx, _slug, ancestor_base)) = contained {
            if fff::git::working_tree_root(std::path::Path::new(&ancestor_base)) == candidate_wt {
                tracing::debug!("master: routing {base_path} to containing root on worker-{idx}");
                return Some(idx);
            }
            tracing::debug!(
                "master: {base_path} is a distinct git working tree from containing root {ancestor_base}; minting its own root"
            );
        }

        // Ring assignment is read-only and deterministic; compute outside any lock.
        // If the ring is empty (workers still starting), spawn one on demand so
        // Handshakes that arrive before n_min workers are ready still get served.
        let index = {
            let ring = self.ring.lock().await;
            ring.assign(std::path::Path::new(base_path))
        };
        let index = match index {
            Some(idx) => idx,
            None => {
                // Single-flight the empty-ring bootstrap so a burst of cold-start
                // Handshakes doesn't each spawn a worker; re-check under the latch.
                let _bootstrap = self.bootstrap.lock().await;
                let reassigned = {
                    let ring = self.ring.lock().await;
                    ring.assign(std::path::Path::new(base_path))
                };
                match reassigned {
                    Some(idx) => idx,
                    None => {
                        let new_idx = self.alloc_index().await;
                        tracing::info!(
                            "master: ring empty, spawning first worker on demand ({new_idx})"
                        );
                        if let Err(e) = self.spawn_worker(new_idx).await {
                            tracing::error!("master: on-demand spawn failed: {e}");
                            return None;
                        }
                        new_idx
                    }
                }
            }
        };

        // Single write-lock: re-check presence, collect now-contained children,
        // push slug, compute scale-out trigger.
        let (should_scale_out, descendants) = {
            let mut routing = self.routing.lock().await;

            // Re-check after lock: a concurrent Handshake may have added this slug already.
            for (idx, entry) in &routing.workers {
                if entry.contains_slug(&slug) {
                    return Some(*idx);
                }
            }

            // Existing roots this new root now lexically contains. Filtered in
            // subsume_descendants to those sharing the new parent's working
            // tree, so a live child worktree is never retracted into it.
            let descendants: Vec<(u32, String, String)> = routing
                .workers
                .iter()
                .flat_map(|(&widx, entry)| {
                    entry
                        .roots
                        .iter()
                        .filter(|r| {
                            !r.base_path.is_empty()
                                && std::path::Path::new(&r.base_path).starts_with(&canonical)
                        })
                        .map(move |r| (widx, r.slug.clone(), r.base_path.clone()))
                })
                .collect();

            let mut scale_out = false;
            if let Some(entry) = routing.workers.get_mut(&index) {
                entry.push_root(slug.clone(), canonical);
                let load = entry.roots.len() as u32;
                let total_workers = routing.workers.len() as u32;
                scale_out =
                    load >= self.config.roots_per_worker_max && total_workers < self.config.n_max;
            }
            // Remove from idle_since: this worker now has work.
            self.idle_since.lock().await.remove(&index);
            self.persist_routing(&routing);
            (scale_out, descendants)
        };

        // Subsume any pre-existing child roots off the hot path (passive, async).
        if !descendants.is_empty() {
            let me = Arc::clone(self);
            let parent_slug = slug.clone();
            let parent_wt = candidate_wt.clone();
            tokio::spawn(async move {
                me.subsume_descendants(parent_slug, parent_wt, descendants)
                    .await;
            });
        }

        if should_scale_out {
            let new_idx = self.alloc_index().await;
            tracing::info!("master: scale-out triggered, spawning worker-{new_idx}");
            if let Err(e) = self.spawn_worker(new_idx).await {
                tracing::error!("master: scale-out spawn failed: {e}");
            }
        }

        Some(index)
    }

    // Subsume pre-existing child roots into a newly-registered parent: tell each
    // child's worker to merge frecency into the parent and drop the child's
    // EngineState, then remove the child from the routing table. Runs in a
    // background task so the triggering Handshake never blocks.
    async fn subsume_descendants(
        &self,
        parent_slug: String,
        parent_wt: Option<std::path::PathBuf>,
        descendants: Vec<(u32, String, String)>,
    ) {
        for (widx, child_slug, child_base) in descendants {
            // A child that is its own git working tree (linked worktree,
            // submodule, nested repo) is a distinct checkout — never retract it
            // into the lexical parent, or an active worktree would be served
            // the parent's stale view.
            if fff::git::working_tree_root(std::path::Path::new(&child_base)) != parent_wt {
                tracing::debug!(
                    "master: keeping child root {child_slug} ({child_base}) — distinct git working tree from {parent_slug}"
                );
                continue;
            }
            let socket = worker_socket_path(widx);
            if let Err(e) = send_drop_root(&socket, child_slug.clone(), parent_slug.clone()).await {
                tracing::warn!(
                    "master: subsumption DropRoot for {child_slug} on worker-{widx} failed: {e}"
                );
            }
            // Remove from routing regardless: future Handshakes for the child
            // path now resolve to the parent via containment.
            self.handle_evicted_root(&child_slug).await;
            tracing::info!("master: subsumed child root {child_slug} into {parent_slug}");
        }
    }
}

// Send a DropRoot to a worker and await its Ack. Used by containment subsumption.
#[cfg(unix)]
async fn send_drop_root(
    socket: &std::path::Path,
    slug: String,
    merge_into_slug: String,
) -> Result<(), String> {
    // Bound the whole round-trip so a wedged or version-skewed worker can't
    // hang this background task (mirrors the client-side timeout discipline).
    tokio::time::timeout(Duration::from_secs(5), async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(|e| e.to_string())?;
        let (mut read_half, mut write_half) = tokio::io::split(stream);
        write_message(
            &mut write_half,
            &SearchRequest::DropRoot {
                slug,
                merge_into_slug,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        let _: SearchResponse = read_message(&mut read_half)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    })
    .await
    .map_err(|_| "DropRoot timed out".to_string())?
}

// Connect to a worker socket, send Health as the first message, and read the
// HealthResponse. Worker closes the connection after responding.
#[cfg(unix)]
async fn query_worker_health(
    socket: &std::path::Path,
) -> Result<fff_ipc::types::HealthResponse, String> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;
    let (mut read_half, mut write_half) = tokio::io::split(stream);
    write_message(&mut write_half, &SearchRequest::Health)
        .await
        .map_err(|e| format!("write failed: {e}"))?;
    match read_message::<_, SearchResponse>(&mut read_half).await {
        Ok(SearchResponse::Health(resp)) => Ok(resp),
        Ok(SearchResponse::Error(msg)) => Err(msg),
        Ok(other) => Err(format!("unexpected response: {other:?}")),
        Err(e) => Err(format!("read failed: {e}")),
    }
}

/// Entry point for master mode.
#[cfg(not(unix))]
pub async fn run(_config: fff_ipc::config::FffConfig) -> Result<(), Box<dyn std::error::Error>> {
    Err("fff-engine master mode is not supported on this platform".into())
}

// Canonicalize a path, falling back to the original when it does not exist.
#[cfg(unix)]
fn canonicalize_root(p: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// Derive default-first `ConfiguredRoot`s from `[mcp]`, mirroring
// `RootRegistry::all_with_names`: canonicalize, default first, de-dup by
// canonical path (first wins).
#[cfg(unix)]
fn configured_roots_from_mcp(mcp: &fff_ipc::config::McpConfig) -> Vec<ConfiguredRoot> {
    let default_path = mcp.default_path().map(|p| canonicalize_root(&p));
    let default_name = default_path.as_ref().and_then(|dp| {
        mcp.roots
            .iter()
            .find(|r| canonicalize_root(&r.path) == *dp)
            .and_then(|r| r.name.clone())
    });

    let mut out: Vec<ConfiguredRoot> = Vec::with_capacity(mcp.roots.len() + 1);
    if let Some(dp) = default_path.clone() {
        out.push(ConfiguredRoot {
            name: default_name,
            path: dp,
            default: true,
        });
    }
    for root in &mcp.roots {
        let canon = canonicalize_root(&root.path);
        if out.iter().any(|e| e.path == canon) {
            continue;
        }
        out.push(ConfiguredRoot {
            name: root.name.clone(),
            path: canon,
            default: false,
        });
    }
    out
}

/// Entry point for master mode.
#[cfg(unix)]
pub async fn run(config: fff_ipc::config::FffConfig) -> Result<(), Box<dyn std::error::Error>> {
    let lockfile = master_lockfile_path();
    if let Some(parent) = lockfile.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // O_CREAT|O_EXCL race — exactly one process wins the master authority.
    use std::fs::OpenOptions;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lockfile)
    {
        Ok(_) => {}
        Err(_) => {
            // Check whether the existing lock is stale.
            if fff_ipc::lockfile::is_stale(&lockfile) {
                tracing::warn!("master: removing stale lockfile");
                let _ = std::fs::remove_file(&lockfile);
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&lockfile)
                    .map_err(|_| "another master is already running")?;
            } else {
                tracing::info!("master: another instance is already running, exiting");
                return Ok(());
            }
        }
    }
    std::fs::write(&lockfile, format!("{}\n", std::process::id()))?;

    let exe_path = std::env::current_exe()?;
    let configured_roots = configured_roots_from_mcp(&config.mcp);
    let idle_root_ttl_secs = config.index.resolved_idle_root_ttl_secs();
    let worker_cfg = config.worker;

    // Load routing.json and probe surviving workers.
    let rt_path = routing_table_path();
    let mut routing = RoutingTable::load(&rt_path).unwrap_or_default();
    let mut adopted_pids: HashMap<u32, u32> = HashMap::new();
    // Track highest worker index seen; used to assign the next index without
    // reusing gaps. Starts at u32::MAX so that wrapping_add(1) yields 0 when
    // no prior workers exist (fresh start).
    let mut max_seen_index: u32 = u32::MAX;

    // Restore ring from persisted snapshot, then remove dead workers.
    // Using from_serializable preserves the exact prior layout even if
    // DEFAULT_VIRTUAL_NODES changes between restarts.
    let mut ring = HashRing::from_serializable(routing.ring_state.clone());
    let mut dead_indices: Vec<u32> = vec![];
    for (&idx, entry) in &routing.workers {
        max_seen_index = max_seen_index.max(idx);
        let pid_alive = unsafe { libc::kill(entry.pid as libc::pid_t, 0) == 0 };
        // Also verify the worker socket is connectable — a recycled PID would pass
        // kill(pid,0) but the dead worker's socket file would be absent or unusable.
        let socket_alive = pid_alive && {
            let sock = worker_socket_path(idx);
            std::os::unix::net::UnixStream::connect(&sock).is_ok()
        };
        if socket_alive {
            adopted_pids.insert(idx, entry.pid);
            tracing::info!("master: reconnected worker-{idx} pid={}", entry.pid);
        } else {
            ring.remove_worker(idx);
            dead_indices.push(idx);
            tracing::info!("master: discarded dead worker-{idx} pid={}", entry.pid);
        }
    }
    for idx in dead_indices {
        routing.workers.remove(&idx);
    }

    let surviving = routing.workers.len() as u32;
    let master_state = Arc::new(MasterState::new(
        worker_cfg.clone(),
        exe_path,
        routing,
        ring,
        max_seen_index.wrapping_add(1),
        adopted_pids,
        configured_roots,
    ));

    // Bind master socket before spawning workers so clients can connect
    // immediately. Workers start in the background; Handshakes that arrive
    // before the ring is populated trigger on-demand spawn inside assign_new_root.
    let socket = master_socket_path();
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!("fff-engine master listening on {}", socket.display());

    // Fill to n_min in the background — don't delay socket availability. Shares
    // the bootstrap latch with on-demand spawn so the two never double-spawn.
    if worker_cfg.n_min > surviving {
        let ms_init = Arc::clone(&master_state);
        let target = worker_cfg.n_min;
        tokio::spawn(async move {
            ms_init.ensure_min_workers(target).await;
        });
    }

    // Background: poll children for crashes and respawn them in parallel.
    // restart_count tracks (attempts, window_start) per worker index.
    // Max 3 restarts per 60s window to prevent restart storms.
    let ms_monitor = Arc::clone(&master_state);
    tokio::spawn(async move {
        let mut restart_count: HashMap<u32, (u32, Instant)> = HashMap::new();
        const MAX_RESTARTS_PER_WINDOW: u32 = 3;
        const RESTART_WINDOW: Duration = Duration::from_secs(60);
        let mut ticker = interval(Duration::from_secs(2));

        loop {
            ticker.tick().await;
            let mut children = ms_monitor.children.lock().await;
            let mut crashed: Vec<u32> = vec![];
            for (&idx, child) in children.iter_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        tracing::warn!("master: worker-{idx} exited: {status}");
                        crashed.push(idx);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::error!("master: worker-{idx} try_wait: {e}"),
                }
            }
            drop(children);

            for idx in crashed {
                ms_monitor.children.lock().await.remove(&idx);

                let now = Instant::now();
                let (prev_count, window_start) = *restart_count.entry(idx).or_insert((0, now));
                let (count, window_start) = if now.duration_since(window_start) > RESTART_WINDOW {
                    (0, now)
                } else {
                    (prev_count, window_start)
                };

                if count >= MAX_RESTARTS_PER_WINDOW {
                    tracing::error!(
                        "master: worker-{idx} crashed {MAX_RESTARTS_PER_WINDOW} times \
                         in {RESTART_WINDOW:?}, removing permanently"
                    );
                    restart_count.remove(&idx);
                    ms_monitor.routing.lock().await.workers.remove(&idx);
                    ms_monitor.ring.lock().await.remove_worker(idx);
                    continue;
                }

                let backoff = Duration::from_millis(100 * (1u64 << count));
                restart_count.insert(idx, (count + 1, window_start));
                tracing::info!(
                    "master: respawning worker-{idx} (attempt {}) after {backoff:?}",
                    count + 1
                );

                // Spawn independent task so N simultaneous crashes respawn in parallel.
                let ms = Arc::clone(&ms_monitor);
                tokio::spawn(async move {
                    sleep(backoff).await;
                    if let Err(e) = ms.spawn_worker(idx).await {
                        tracing::error!("master: failed to respawn worker-{idx}: {e}");
                        ms.routing.lock().await.workers.remove(&idx);
                        ms.ring.lock().await.remove_worker(idx);
                    }
                });
            }
        }
    });

    // Background: idle TTL reaper. First evicts idle/stale on-demand roots
    // (freeing their worker slots), then stops workers left with no loaded roots
    // after idle_ttl_secs. Configured roots are exempt from eviction.
    let ms_idle = Arc::clone(&master_state);
    let idle_ttl = Duration::from_secs(worker_cfg.idle_ttl_secs);
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;

            // Phase 1: evict stale on-demand roots. Path-gone roots (deleted dir
            // or dangling worktree) are reaped every tick regardless of TTL;
            // idle-age eviction additionally applies when idle_root_ttl_secs > 0.
            // Decide under no lock (health snapshot + pure predicate), then tear
            // each victim down without holding a lock across the worker DropRoot
            // RPC. A worker left empty here is stopped by Phase 2 once idle_ttl
            // elapses.
            let report = ms_idle.collect_health().await;
            let victims = roots_to_evict(
                &report,
                &ms_idle.configured_slugs,
                idle_root_ttl_secs,
                root_path_gone,
            );
            for (widx, slug) in victims {
                let socket = worker_socket_path(widx);
                if let Err(e) = send_drop_root(&socket, slug.clone(), String::new()).await {
                    tracing::warn!(
                        "master: eviction DropRoot for {slug} on worker-{widx} failed: {e}"
                    );
                }
                ms_idle.handle_evicted_root(&slug).await;
                tracing::info!(
                    "master: evicted idle/stale on-demand root {slug} from worker-{widx}"
                );
            }

            // Phase 2: stop workers with no loaded roots after idle_ttl_secs.
            let now = Instant::now();
            let mut to_stop: Vec<u32> = vec![];
            {
                let routing = ms_idle.routing.lock().await;
                let mut idle = ms_idle.idle_since.lock().await;
                for &idx in routing.workers.keys() {
                    let entry_count = routing.entries_for_worker(idx);
                    if entry_count == 0 {
                        let since = idle.entry(idx).or_insert(now);
                        if now.duration_since(*since) >= idle_ttl {
                            to_stop.push(idx);
                        }
                    } else {
                        idle.remove(&idx);
                    }
                }
            }
            for idx in to_stop {
                tracing::info!("master: worker-{idx} idle TTL elapsed, stopping");
                ms_idle.stop_worker(idx).await;
                ms_idle.idle_since.lock().await.remove(&idx);
            }
        }
    });

    // Background: re-probe adopted workers every 30s; respawn any that have died.
    // Crash monitor only watches children (spawned this session); adopted workers
    // have no Child handle and are invisible to try_wait().
    let ms_adopted = Arc::clone(&master_state);
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(30));
        loop {
            ticker.tick().await;
            let mut dead: Vec<u32> = vec![];
            {
                let adopted = ms_adopted.adopted_pids.lock().await;
                for (&idx, &pid) in &*adopted {
                    let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
                    if !alive {
                        tracing::warn!(
                            "master: adopted worker-{idx} (pid={pid}) is no longer alive"
                        );
                        dead.push(idx);
                    }
                }
            }
            for idx in dead {
                ms_adopted.adopted_pids.lock().await.remove(&idx);
                {
                    let mut routing = ms_adopted.routing.lock().await;
                    routing.workers.remove(&idx);
                    ms_adopted.persist_routing(&routing);
                }
                ms_adopted.ring.lock().await.remove_worker(idx);
                if let Err(e) = ms_adopted.spawn_worker(idx).await {
                    tracing::error!("master: failed to respawn adopted worker-{idx}: {e}");
                }
            }
        }
    });

    // Main accept loop.
    let shutdown = async {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("master: SIGINT"),
            _ = sigterm.recv() => tracing::info!("master: SIGTERM"),
        }
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let ms = Arc::clone(&master_state);
                        tokio::spawn(handle_connection(stream, ms));
                    }
                    Err(e) => tracing::error!("master: accept: {e}"),
                }
            }
            _ = &mut shutdown => break,
        }
    }

    // Propagate shutdown to all workers via SIGTERM.
    {
        let mut children = master_state.children.lock().await;
        for (idx, child) in children.drain() {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                tracing::info!("master: sent SIGTERM to worker-{idx} pid={pid}");
            }
        }
    }
    {
        let adopted = master_state.adopted_pids.lock().await;
        for (&idx, &pid) in adopted.iter() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
            tracing::info!("master: sent SIGTERM to adopted worker-{idx}");
        }
    }

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&lockfile);
    tracing::info!("master: stopped");
    Ok(())
}

#[cfg(unix)]
async fn handle_connection(stream: tokio::net::UnixStream, ms: Arc<MasterState>) {
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    // Dual-read: read one frame, sniff the first payload byte. `{` ⇒ versioned
    // JSON envelope; anything else ⇒ legacy bincode (byte-for-byte unchanged).
    let frame = match read_frame(&mut read_half).await {
        Ok(f) => f,
        Err(_) => return,
    };

    if looks_like_json(&frame) {
        let resp = dispatch_master_json(&frame, &ms).await;
        let _ = write_json_message(&mut write_half, &resp).await;
        return;
    }

    let req: MasterRequest = match decode_bincode(&frame) {
        Ok(r) => r,
        Err(_) => return,
    };

    match req {
        MasterRequest::Handshake { base_path } => {
            let resp = match ms.resolve_worker_socket(&base_path).await {
                Ok((path, worker_index)) => MasterResponse::WorkerSocket { path, worker_index },
                Err(msg) => MasterResponse::Error(msg),
            };
            let _ = write_message(&mut write_half, &resp).await;
        }

        MasterRequest::RouteInfo { base_path } => {
            let ring = ms.ring.lock().await;
            let resp = match ring.assign(std::path::Path::new(&base_path)) {
                Some(index) => {
                    drop(ring);
                    if let Some(info) = ms.worker_info(index).await {
                        MasterResponse::WorkerInfo(info)
                    } else {
                        MasterResponse::Error(format!("worker-{index} not found"))
                    }
                }
                None => {
                    drop(ring);
                    MasterResponse::Error("ring is empty".into())
                }
            };
            let _ = write_message(&mut write_half, &resp).await;
        }

        MasterRequest::ListWorkers => {
            let workers = ms.collect_worker_info().await;
            let _ = write_message(&mut write_half, &MasterResponse::WorkerList { workers }).await;
        }

        MasterRequest::WorkerStatus { index } => {
            let resp = match ms.worker_info(index).await {
                Some(info) => MasterResponse::WorkerInfo(info),
                None => MasterResponse::Error(format!("worker-{index} not found")),
            };
            let _ = write_message(&mut write_half, &resp).await;
        }

        MasterRequest::StopWorker { index } => {
            ms.stop_worker(index).await;
            let _ = write_message(&mut write_half, &MasterResponse::Ack).await;
        }

        MasterRequest::EvictedRoot { slug } => {
            // Fire-and-forget: no response sent.
            ms.handle_evicted_root(&slug).await;
        }

        MasterRequest::Health => {
            let report = ms.collect_health().await;
            let _ = write_message(&mut write_half, &MasterResponse::HealthReport(report)).await;
        }
    }
}

// JSON dual-read dispatch for the master socket. Parses the versioned envelope,
// enforces the version check (refuse-on-mismatch, R3/KTD4), then routes the verb
// through the SAME helpers the bincode path uses. Returns the response envelope
// to write; the caller closes the connection after a single request, matching
// the bincode path's one-shot lifecycle.
#[cfg(unix)]
async fn dispatch_master_json(frame: &[u8], ms: &Arc<MasterState>) -> ResponseEnvelope {
    let env: RequestEnvelope = match decode_json(frame) {
        Ok(e) => e,
        Err(e) => {
            return ResponseEnvelope::err(ResponseError {
                code: BAD_REQUEST.to_string(),
                message: format!("malformed request envelope: {e}"),
                engine_version: None,
                client_version: None,
            });
        }
    };

    if let Err(mismatch) = check_protocol_version(env.protocol_version) {
        return mismatch;
    }

    match env.verb.as_str() {
        verbs::HANDSHAKE => {
            let params: BasePathParams = match env.params_as() {
                Ok(p) => p,
                Err(e) => return e,
            };
            match ms.resolve_worker_socket(&params.base_path).await {
                Ok((worker_socket, worker_index)) => {
                    let result = HandshakeResult {
                        worker_socket,
                        worker_index,
                    };
                    ok_or_internal(ResponseEnvelope::ok(&result))
                }
                Err(message) => ResponseEnvelope::err(ResponseError {
                    code: INTERNAL.to_string(),
                    message,
                    engine_version: None,
                    client_version: None,
                }),
            }
        }

        verbs::HEALTH => {
            let report = ms.collect_health().await;
            ok_or_internal(ResponseEnvelope::ok(&report))
        }

        verbs::LIST_ROOTS => {
            let live: Vec<String> = ms
                .collect_worker_info()
                .await
                .into_iter()
                .flat_map(|w| w.roots.into_iter().map(|r| r.base_path))
                .collect();
            let roots = build_list_roots(&ms.configured_roots, &live);
            ok_or_internal(ResponseEnvelope::ok(&roots))
        }

        other => ResponseEnvelope::err(ResponseError {
            code: BAD_REQUEST.to_string(),
            message: format!("unsupported master verb: {other}"),
            engine_version: None,
            client_version: None,
        }),
    }
}

// Reconcile configured `[mcp]` roots with live routing-table base_paths into the
// `list_roots` result. Configured roots come first (already default-first and
// self-de-duped); live-only base_paths follow as `name:None`/`default:false`.
// De-dup is by canonical path — a configured root that is also live appears once
// and keeps its name/default.
#[cfg(unix)]
fn build_list_roots(configured: &[ConfiguredRoot], live: &[String]) -> Vec<WireRoot> {
    let mut out: Vec<WireRoot> = configured
        .iter()
        .map(|c| WireRoot {
            base_path: c.path.to_string_lossy().into_owned(),
            name: c.name.clone(),
            default: c.default,
        })
        .collect();

    let mut seen: Vec<PathBuf> = configured.iter().map(|c| c.path.clone()).collect();
    for base_path in live {
        let canon = canonicalize_root(std::path::Path::new(base_path));
        if seen.contains(&canon) {
            continue;
        }
        out.push(WireRoot {
            base_path: canon.to_string_lossy().into_owned(),
            name: None,
            default: false,
        });
        seen.push(canon);
    }
    out
}

// Decide which loaded roots to evict this reaper tick. Configured roots (by
// slug) are always exempt. Path-gone eviction (`path_gone`) is unconditional; an
// on-demand root also goes when unqueried for `idle_ttl_secs`. `idle_ttl_secs ==
// 0` disables only the idle-age branch — path-gone roots are still reaped. Pure
// over its inputs — the clock lives in the health snapshot and the filesystem in
// the closure — so it unit-tests without a running daemon.
#[cfg(unix)]
fn roots_to_evict(
    report: &HealthReport,
    configured_slugs: &HashSet<String>,
    idle_ttl_secs: u64,
    path_gone: impl Fn(&str) -> bool,
) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for w in &report.workers {
        for r in &w.roots {
            if configured_slugs.contains(&r.slug) {
                continue;
            }
            let idle_expired = idle_ttl_secs > 0
                && r.last_access_age_sec.is_some_and(|age| age >= idle_ttl_secs);
            if idle_expired || path_gone(&r.base_path) {
                out.push((w.index, r.slug.clone()));
            }
        }
    }
    out
}

// A root's path is gone if the directory no longer exists, or it is a dangling
// git worktree — a `.git` marker present yet no working tree resolves (e.g.
// `git worktree remove` deleted the admin dir but left the checkout). A path
// that was never a git root (no `.git` marker) is left to idle eviction only,
// so legitimate non-repo roots are never reaped for lacking a working tree.
#[cfg(unix)]
fn root_path_gone(base_path: &str) -> bool {
    let p = std::path::Path::new(base_path);
    // Only a CONFIRMED not-found counts as gone. Any other stat error
    // (EACCES, I/O, an unreachable network mount, a mid-flight worktree/rebase
    // rewriting `.git`) is transient — keep the root rather than reap it.
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
        Ok(_) => {}
    }
    // Dangling git worktree: a `.git` marker present yet no working tree
    // resolves. Reached only once the path is confirmed present above, so this
    // fires on a real resolution failure, not a transient stat error.
    p.join(".git").exists() && fff::git::working_tree_root(p).is_none()
}

// Collapse a result-serialization failure into an INTERNAL error envelope.
#[cfg(unix)]
fn ok_or_internal<E: std::fmt::Display>(r: Result<ResponseEnvelope, E>) -> ResponseEnvelope {
    r.unwrap_or_else(|e| {
        ResponseEnvelope::err(ResponseError {
            code: INTERNAL.to_string(),
            message: format!("failed to serialize result: {e}"),
            engine_version: None,
            client_version: None,
        })
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use fff_ipc::types::HealthReport;
    use fff_ipc::types::RootHealth;
    use fff_ipc::{PROTOCOL_MISMATCH, PROTOCOL_VERSION};
    use serde_json::json;

    fn health_root(slug: &str, base_path: &str, last_access_age_sec: Option<u64>) -> RootHealth {
        RootHealth {
            slug: slug.into(),
            base_path: base_path.into(),
            indexed_files: None,
            last_scan_age_sec: None,
            watcher_backlog: None,
            dirty_count: None,
            last_access_age_sec,
        }
    }

    // One worker (index 7) holding the given roots.
    fn health_report(roots: Vec<RootHealth>) -> HealthReport {
        HealthReport {
            master_pid: 1,
            uptime_sec: 0,
            workers: vec![WorkerHealth {
                index: 7,
                pid: 100,
                socket_path: "w.sock".into(),
                roots,
            }],
        }
    }

    #[test]
    fn evicts_root_idle_past_ttl() {
        let report = health_report(vec![health_root("idle", "/proj/idle", Some(7200))]);
        let evict = roots_to_evict(&report, &HashSet::new(), 3600, |_| false);
        assert_eq!(evict, vec![(7, "idle".to_string())]);
    }

    #[test]
    fn keeps_root_queried_within_ttl() {
        let report = health_report(vec![health_root("hot", "/proj/hot", Some(60))]);
        let evict = roots_to_evict(&report, &HashSet::new(), 3600, |_| false);
        assert!(evict.is_empty());
    }

    #[test]
    fn evicts_path_gone_root_regardless_of_idle() {
        // Queried seconds ago, but its path is gone → evicted immediately.
        let report = health_report(vec![health_root("gone", "/proj/gone", Some(0))]);
        let evict = roots_to_evict(&report, &HashSet::new(), 3600, |bp| bp == "/proj/gone");
        assert_eq!(evict, vec![(7, "gone".to_string())]);
    }

    #[test]
    fn never_evicts_configured_root() {
        // Idle AND path-gone, but configured → exempt from both.
        let configured: HashSet<String> = ["cfg".to_string()].into_iter().collect();
        let report = health_report(vec![health_root("cfg", "/proj/cfg", Some(99999))]);
        let evict = roots_to_evict(&report, &configured, 3600, |_| true);
        assert!(evict.is_empty());
    }

    #[test]
    fn ttl_zero_keeps_idle_but_evicts_path_gone() {
        // ttl=0 disables idle-age eviction only: the long-idle root is kept, but
        // the path-gone root (queried seconds ago) is still reaped.
        let report = health_report(vec![
            health_root("idle", "/proj/idle", Some(99999)),
            health_root("gone", "/proj/gone", Some(0)),
        ]);
        let evict = roots_to_evict(&report, &HashSet::new(), 0, |bp| bp == "/proj/gone");
        assert_eq!(evict, vec![(7, "gone".to_string())]);
    }

    // A master state with one worker already holding `base_path`, so a handshake
    // routing hit returns without spawning a real worker process.
    fn state_with_root(base_path: &str) -> Arc<MasterState> {
        let mut routing = RoutingTable::default();
        let mut entry = WorkerEntry::new(0, worker_socket_path(0).to_string_lossy().into(), 1234);
        let slug = base_path_slug(std::path::Path::new(base_path));
        entry.push_root(slug, base_path.to_string());
        routing.workers.insert(0, entry);

        Arc::new(MasterState::new(
            WorkerConfig::default(),
            PathBuf::from("/nonexistent/fff-engine"),
            routing,
            HashRing::new(),
            1,
            HashMap::new(),
            Vec::new(),
        ))
    }

    fn frame_for(env: &RequestEnvelope) -> Vec<u8> {
        serde_json::to_vec(env).unwrap()
    }

    #[tokio::test]
    async fn json_handshake_returns_handshake_result() {
        let base_path = "/tmp/fff-dual-read-test-root";
        let ms = state_with_root(base_path);
        let env = RequestEnvelope::new(verbs::HANDSHAKE, json!({ "base_path": base_path }));

        let resp = dispatch_master_json(&frame_for(&env), &ms).await;

        assert!(resp.ok, "expected ok, got {resp:?}");
        assert_eq!(resp.protocol_version, PROTOCOL_VERSION);
        let result: HandshakeResult = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(result.worker_index, 0);
        assert_eq!(
            result.worker_socket,
            worker_socket_path(0).to_string_lossy()
        );
    }

    #[tokio::test]
    async fn json_handshake_version_mismatch_refuses_loud() {
        let base_path = "/tmp/fff-dual-read-test-root";
        let ms = state_with_root(base_path);
        let mut env = RequestEnvelope::new(verbs::HANDSHAKE, json!({ "base_path": base_path }));
        env.protocol_version = PROTOCOL_VERSION + 1;

        let resp = dispatch_master_json(&frame_for(&env), &ms).await;

        assert!(!resp.ok);
        assert!(
            resp.result.is_none(),
            "must not return a worker socket on skew"
        );
        let err = resp.error.unwrap();
        assert_eq!(err.code, PROTOCOL_MISMATCH);
        assert_eq!(err.engine_version, Some(PROTOCOL_VERSION));
        assert_eq!(err.client_version, Some(PROTOCOL_VERSION + 1));
    }

    #[tokio::test]
    async fn json_health_returns_health_report() {
        // Empty routing table → no worker fan-out, report has no workers.
        let ms = Arc::new(MasterState::new(
            WorkerConfig::default(),
            PathBuf::from("/nonexistent/fff-engine"),
            RoutingTable::default(),
            HashRing::new(),
            0,
            HashMap::new(),
            Vec::new(),
        ));
        let env = RequestEnvelope::new(verbs::HEALTH, json!({}));

        let resp = dispatch_master_json(&frame_for(&env), &ms).await;

        assert!(resp.ok, "expected ok, got {resp:?}");
        let report: HealthReport = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(report.master_pid, std::process::id());
        assert!(report.workers.is_empty());
    }

    #[tokio::test]
    async fn json_unknown_verb_is_bad_request() {
        let ms = state_with_root("/tmp/fff-dual-read-test-root");
        let env = RequestEnvelope::new("not_a_real_verb", json!({}));

        let resp = dispatch_master_json(&frame_for(&env), &ms).await;

        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, BAD_REQUEST);
        assert!(err.message.contains("unsupported master verb"));
        assert!(err.message.contains("not_a_real_verb"));
    }

    #[tokio::test]
    async fn json_list_roots_returns_live_base_path() {
        let base_path = "/tmp/fff-dual-read-test-root";
        let ms = state_with_root(base_path);
        let env = RequestEnvelope::new(verbs::LIST_ROOTS, json!({}));

        let resp = dispatch_master_json(&frame_for(&env), &ms).await;

        assert!(resp.ok, "expected ok, got {resp:?}");
        let roots: Vec<WireRoot> = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, None);
        assert!(!roots[0].default);
    }

    fn configured(name: Option<&str>, path: &str, default: bool) -> ConfiguredRoot {
        ConfiguredRoot {
            name: name.map(|n| n.to_string()),
            path: PathBuf::from(path),
            default,
        }
    }

    #[test]
    fn build_list_roots_unions_configured_and_live() {
        let configured_roots = vec![
            configured(Some("app"), "/tmp/fff-cfg-app", true),
            configured(Some("lib"), "/tmp/fff-cfg-lib", false),
        ];
        let live = vec!["/tmp/fff-live-only".to_string()];

        let roots = build_list_roots(&configured_roots, &live);

        assert_eq!(roots.len(), 3);
        // Default-first, configured carry name/default.
        assert_eq!(roots[0].base_path, "/tmp/fff-cfg-app");
        assert_eq!(roots[0].name.as_deref(), Some("app"));
        assert!(roots[0].default);
        assert_eq!(roots[1].name.as_deref(), Some("lib"));
        assert!(!roots[1].default);
        // Live-only is anonymous and non-default.
        assert_eq!(roots[2].base_path, "/tmp/fff-live-only");
        assert_eq!(roots[2].name, None);
        assert!(!roots[2].default);
    }

    #[test]
    fn build_list_roots_dedups_configured_that_is_also_live() {
        // The path exists so both sides canonicalize identically. The configured
        // path is pre-canonicalized (as `configured_roots_from_mcp` does in prod).
        let tmp = tempfile::tempdir().unwrap();
        let raw = tmp.path().to_string_lossy().into_owned();
        let canon = canonicalize_root(std::path::Path::new(&raw))
            .to_string_lossy()
            .into_owned();

        let configured_roots = vec![configured(Some("app"), &canon, true)];
        let live = vec![raw.clone()];

        let roots = build_list_roots(&configured_roots, &live);

        assert_eq!(roots.len(), 1, "de-dup by canonical path");
        assert_eq!(roots[0].name.as_deref(), Some("app"));
        assert!(roots[0].default, "configured entry wins");
    }

    #[test]
    fn build_list_roots_empty_config_single_live() {
        let roots = build_list_roots(&[], &["/tmp/fff-live-solo".to_string()]);

        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].base_path, "/tmp/fff-live-solo");
        assert_eq!(roots[0].name, None);
        assert!(!roots[0].default);
    }

    // Regression (R5): the legacy bincode Handshake → WorkerSocket round-trip is
    // byte-for-byte unchanged. We exercise the shared routing helper the bincode
    // arm now calls, plus confirm the frame still sniffs as bincode (not JSON).
    #[tokio::test]
    async fn legacy_bincode_handshake_still_routes() {
        let base_path = "/tmp/fff-dual-read-test-root";
        let ms = state_with_root(base_path);

        // Encode the legacy request exactly as a bincode client would.
        let req = MasterRequest::Handshake {
            base_path: base_path.to_string(),
        };
        let frame = bincode::serialize(&req).unwrap();
        assert!(
            !looks_like_json(&frame),
            "legacy bincode frame must not sniff as JSON"
        );

        // The bincode arm routes through resolve_worker_socket; assert the same
        // (path, index) the WorkerSocket response would carry.
        let (path, index) = ms.resolve_worker_socket(base_path).await.unwrap();
        assert_eq!(index, 0);
        assert_eq!(path, worker_socket_path(0).to_string_lossy());

        // And the legacy decode path still yields the original request.
        let decoded: MasterRequest = decode_bincode(&frame).unwrap();
        match decoded {
            MasterRequest::Handshake { base_path: bp } => assert_eq!(bp, base_path),
            other => panic!("expected Handshake, got {other:?}"),
        }
    }
}
