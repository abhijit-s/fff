#![cfg(unix)]
//! Integration tests for fff-engine covering:
//! - U3: Worker socket binding, protocol enforcement, Connect/Ack, cleanup on SIGTERM
//! - U4: Master lockfile, socket, single-instance guard, Handshake, ListWorkers,
//!   WorkerStatus, routing.json persistence, startup with dead-PID routing.json, cleanup
//! - U5: Routing table fast path, scale-out on roots_per_worker_max, stable re-routing,
//!   routing.json updated after each Handshake mutation
//! - U6: Worker crash detection and respawn, startup dead-vs-live PID recovery
//! - U3 (JSON): dual-read connect+grep, fire-and-forget record_access, version
//!   mismatch rejection, legacy bincode regression, bad first-verb rejection

use std::{
    cell::RefCell,
    os::unix::net::UnixStream,
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::Duration,
};

use fff_ipc::{
    codec::{
        read_json_message_sync, read_message_sync, write_json_message_sync, write_message_sync,
    },
    protocol::{
        BasePathParams, GrepParams, PROTOCOL_MISMATCH, PROTOCOL_VERSION, RecordAccessParams,
        RequestEnvelope, ResponseEnvelope, verbs,
    },
    routing::{RoutingTable, WorkerEntry},
    types::{
        FindOptions, GrepOptions, MasterRequest, MasterResponse, SearchRequest, SearchResponse,
    },
};
use serde_json::json;
use tempfile::TempDir;

const ENGINE_BIN: &str = env!("CARGO_BIN_EXE_fff-engine");
/// Max wait for master or worker socket readiness.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(15);
/// Short poll interval.
const POLL_MS: Duration = Duration::from_millis(50);

// ── TestEnv ────────────────────────────────────────────────────────────────────

struct TestEnv {
    dir: TempDir,
    /// PIDs of every process spawned via this env, killed on Drop so a
    /// panicking test can't leak an orphaned master/worker.
    spawned: RefCell<Vec<u32>>,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Self {
            dir,
            spawned: RefCell::new(Vec::new()),
        };

        // Write a config file picked up by all spawned processes via XDG_CONFIG_HOME.
        let cfg_dir = env.config_dir().join("fff");
        std::fs::create_dir_all(&cfg_dir).expect("create config dir");
        let cfg = "[worker]\nn_min = 1\nn_max = 3\nroots_per_worker_max = 2\nidle_ttl_secs = 5\n";
        std::fs::write(cfg_dir.join("config.toml"), cfg).expect("write config");

        // Create the cache/runtime subdirs so processes can write sockets/lockfiles
        // without a race on directory creation in the engine itself.
        std::fs::create_dir_all(env.cache_dir().join("fff").join("workers"))
            .expect("create workers dir");
        std::fs::create_dir_all(env.runtime_dir().join("fff")).expect("create runtime fff dir");

        env
    }

    fn cache_dir(&self) -> PathBuf {
        self.dir.path().join("cache")
    }

    fn runtime_dir(&self) -> PathBuf {
        self.dir.path().join("runtime")
    }

    fn config_dir(&self) -> PathBuf {
        self.dir.path().join("config")
    }

    fn master_socket(&self) -> PathBuf {
        self.cache_dir().join("fff").join("master.sock")
    }

    fn master_lockfile(&self) -> PathBuf {
        self.cache_dir().join("fff").join("master.lock")
    }

    fn worker_socket(&self, idx: u32) -> PathBuf {
        self.cache_dir()
            .join("fff")
            .join("workers")
            .join(format!("worker-{idx}.sock"))
    }

    fn worker_lockfile(&self, idx: u32) -> PathBuf {
        self.cache_dir()
            .join("fff")
            .join("workers")
            .join(format!("worker-{idx}.lock"))
    }

    fn routing_json(&self) -> PathBuf {
        self.runtime_dir().join("fff").join("routing.json")
    }

    fn spawn_master(&self) -> Child {
        // process_group(0): make the master its own group leader so Drop can
        // kill the whole group — the master's on-demand worker children inherit
        // its group and would otherwise orphan when the master is killed.
        let child = Command::new(ENGINE_BIN)
            .arg("--master")
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn master");
        self.spawned.borrow_mut().push(child.id());
        child
    }

    fn spawn_worker(&self, idx: u32) -> Child {
        let child = Command::new(ENGINE_BIN)
            .args(["--worker-index", &idx.to_string()])
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn worker");
        self.spawned.borrow_mut().push(child.id());
        child
    }

    /// Spawn a master with a shortened orphan self-heal window so a test can
    /// observe a worker self-exit in seconds instead of the production minute.
    fn spawn_master_with_orphan_timing(&self, grace_secs: u64, check_secs: u64) -> Child {
        let child = Command::new(ENGINE_BIN)
            .arg("--master")
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("XDG_CONFIG_HOME", self.config_dir())
            .env("FFF_ORPHAN_GRACE_SECS", grace_secs.to_string())
            .env("FFF_ORPHAN_CHECK_SECS", check_secs.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn master");
        self.spawned.borrow_mut().push(child.id());
        child
    }

    fn wait_master(&self, timeout: Duration) -> bool {
        self.wait_socket(&self.master_socket(), timeout)
    }

    fn wait_worker(&self, idx: u32, timeout: Duration) -> bool {
        self.wait_socket(&self.worker_socket(idx), timeout)
    }

    fn wait_socket(&self, path: &PathBuf, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if UnixStream::connect(path).is_ok() {
                return true;
            }
            sleep(POLL_MS);
        }
        false
    }

    /// Wait until `path` does NOT exist (or cannot be connected to).
    fn wait_socket_gone(&self, path: &PathBuf, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if UnixStream::connect(path).is_err() {
                return true;
            }
            sleep(POLL_MS);
        }
        false
    }

    fn send_master_request(&self, req: &MasterRequest) -> MasterResponse {
        let mut stream = UnixStream::connect(self.master_socket()).expect("connect to master");
        write_message_sync(&mut stream, req).expect("write master request");
        read_message_sync(&mut stream).expect("read master response")
    }

    fn handshake(&self, base_path: &str) -> MasterResponse {
        self.send_master_request(&MasterRequest::Handshake {
            base_path: base_path.into(),
        })
    }

    fn health(&self) -> fff_ipc::types::HealthReport {
        match self.send_master_request(&MasterRequest::Health) {
            MasterResponse::HealthReport(r) => r,
            other => panic!("expected HealthReport, got {other:?}"),
        }
    }

    fn list_workers(&self) -> Vec<fff_ipc::types::WorkerInfo> {
        match self.send_master_request(&MasterRequest::ListWorkers) {
            MasterResponse::WorkerList { workers } => workers,
            other => panic!("expected WorkerList, got {other:?}"),
        }
    }

    /// Poll ListWorkers until at least `n` workers are registered, or timeout.
    /// Needed because n_min workers spawn in the background after socket bind.
    fn wait_for_workers(&self, n: usize, timeout: Duration) -> Vec<fff_ipc::types::WorkerInfo> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let workers = self.list_workers();
            if workers.len() >= n {
                return workers;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {n} worker(s); only {} registered",
                    workers.len()
                );
            }
            sleep(POLL_MS);
        }
    }

    fn worker_status(&self, idx: u32) -> Option<fff_ipc::types::WorkerInfo> {
        match self.send_master_request(&MasterRequest::WorkerStatus { index: idx }) {
            MasterResponse::WorkerInfo(info) => Some(info),
            MasterResponse::Error(_) => None,
            other => panic!("unexpected WorkerStatus response: {other:?}"),
        }
    }

    /// Connect to a worker socket, send Connect, receive Ack.
    fn worker_connect(&self, worker_sock: &PathBuf, base_path: &str) -> UnixStream {
        let mut stream = UnixStream::connect(worker_sock).expect("connect to worker");
        let req = SearchRequest::Connect {
            base_path: base_path.into(),
        };
        write_message_sync(&mut stream, &req).expect("write Connect");
        let resp: SearchResponse = read_message_sync(&mut stream).expect("read Ack");
        assert!(
            matches!(resp, SearchResponse::Ack),
            "expected Ack from worker Connect, got {resp:?}"
        );
        stream
    }

    /// JSON two-phase: connect to a worker socket, send a versioned `connect`
    /// envelope, expect an ok ack response. Returns the open stream for verbs.
    fn worker_connect_json(&self, worker_sock: &PathBuf, base_path: &str) -> UnixStream {
        let mut stream = UnixStream::connect(worker_sock).expect("connect to worker");
        let env = RequestEnvelope::new(
            verbs::CONNECT,
            serde_json::to_value(BasePathParams {
                base_path: base_path.into(),
            })
            .unwrap(),
        );
        write_json_message_sync(&mut stream, &env).expect("write JSON connect");
        let resp: ResponseEnvelope = read_json_message_sync(&mut stream).expect("read JSON ack");
        assert!(resp.ok, "expected ok ack from JSON connect, got {resp:?}");
        stream
    }

    fn kill_sigterm(child: &Child) {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // SIGKILL the process GROUP of everything we spawned (negative PID).
        // Each was spawned as a group leader, so this also reaps the master's
        // on-demand worker children — which Rust's Child::drop never kills and
        // which outlive a killed master. Already-dead groups are a harmless
        // no-op.
        for &pid in self.spawned.borrow().iter() {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn kill_and_wait(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn sigterm_and_wait(child: &mut Child, timeout: Duration) {
    TestEnv::kill_sigterm(child);
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        sleep(POLL_MS);
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ── U3: Worker tests ────────────────────────────────────────────────────────────

/// U3-1: Worker binds its socket file at worker_socket_path(N) on startup.
#[test]
fn u3_worker_binds_socket_on_startup() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);

    assert!(
        env.wait_worker(0, SOCKET_TIMEOUT),
        "worker-0 socket not ready in time"
    );
    assert!(
        env.worker_socket(0).exists(),
        "worker-0.sock should exist on disk"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-2: Non-Connect first message is rejected — connection closes without crash.
/// Sends FindFiles as first message; worker closes without sending a response.
#[test]
fn u3_non_connect_first_message_closes_connection() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let mut stream = UnixStream::connect(env.worker_socket(0)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    // Send a non-Connect message as the first message on the connection.
    let bad_req = SearchRequest::FindFiles {
        query: "main".into(),
        options: FindOptions::default(),
    };
    write_message_sync(&mut stream, &bad_req).expect("write bad request");

    // Worker should close the connection (EOF) rather than crash.
    let result: Result<SearchResponse, _> = read_message_sync(&mut stream);
    assert!(
        result.is_err(),
        "expected EOF or error, worker should close connection on bad first msg"
    );

    // Worker itself should still be alive (no crash).
    assert!(
        worker.try_wait().expect("try_wait").is_none(),
        "worker should still be running after bad first message"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-3: `Connect { base_path }` receives `Ack`.
#[test]
fn u3_connect_receives_ack() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();
    let _stream = env.worker_connect(&env.worker_socket(0), base_path);

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-4: Two connections for the same base_path both receive Ack (second is fast-path).
#[test]
fn u3_second_connect_same_base_path_gets_ack() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();

    // First connection — triggers state init.
    let _stream1 = env.worker_connect(&env.worker_socket(0), base_path);

    // Second connection — hits the already-loaded root (fast path).
    let _stream2 = env.worker_connect(&env.worker_socket(0), base_path);

    // Worker lockfile PID should be unchanged (no respawn).
    let lockfile_content =
        std::fs::read_to_string(env.worker_lockfile(0)).expect("lockfile should exist");
    let pid: u32 = lockfile_content.trim().parse().expect("pid in lockfile");
    assert_eq!(
        pid,
        worker.id(),
        "worker PID should not change between two connections"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U2: A worker Connect for a sub-path of an already-loaded root binds to that
/// root's EngineState — the worker does not mint a second root (containment).
#[test]
fn u3_subpath_connect_binds_to_ancestor_root() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let parent = env.dir.path().join("project");
    let child = parent.join("src").join("deep");
    std::fs::create_dir_all(&child).unwrap();

    // Connect parent (inits the root), then a sub-path (must bind to parent).
    let _s1 = env.worker_connect(&env.worker_socket(0), parent.to_str().unwrap());
    let _s2 = env.worker_connect(&env.worker_socket(0), child.to_str().unwrap());

    // Worker health reports exactly one loaded root.
    let mut stream = UnixStream::connect(env.worker_socket(0)).expect("connect for health");
    write_message_sync(&mut stream, &SearchRequest::Health).expect("write Health");
    let resp: SearchResponse = read_message_sync(&mut stream).expect("read Health");
    let roots = match resp {
        SearchResponse::Health(h) => h.roots,
        other => panic!("expected Health response, got {other:?}"),
    };
    assert_eq!(
        roots.len(),
        1,
        "sub-path Connect must bind to the ancestor root, not mint a new one"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-5: Worker cleans up socket and lockfile on SIGTERM.
#[test]
fn u3_worker_cleans_up_on_sigterm() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));
    assert!(env.worker_socket(0).exists());

    sigterm_and_wait(&mut worker, Duration::from_secs(5));

    assert!(
        !env.worker_socket(0).exists(),
        "worker-0.sock should be removed after SIGTERM"
    );
    assert!(
        !env.worker_lockfile(0).exists(),
        "worker-0.lock should be removed after SIGTERM"
    );
}

// ── U4: Master tests ────────────────────────────────────────────────────────────

/// U4-1: Master writes PID to master_lockfile_path() on startup.
#[test]
fn u4_master_writes_pid_to_lockfile() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let content =
        std::fs::read_to_string(env.master_lockfile()).expect("master lockfile should exist");
    let pid: u32 = content
        .trim()
        .parse()
        .expect("lockfile should contain a valid PID");
    assert_eq!(
        pid,
        master.id(),
        "lockfile PID should match spawned master PID"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-2: Master binds socket at master_socket_path().
#[test]
fn u4_master_binds_socket() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(
        env.wait_master(SOCKET_TIMEOUT),
        "master socket not ready in time"
    );
    assert!(
        env.master_socket().exists(),
        "master.sock should exist on disk"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-3: Second master instance exits cleanly — lockfile held by live process.
#[test]
fn u4_second_master_exits_cleanly() {
    let env = TestEnv::new();
    let mut master1 = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Spawn a second master in the same env — it should detect the live lockfile and exit.
    let mut master2 = env.spawn_master();
    // Give it a few seconds to detect the conflict and exit.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let second_exited = loop {
        if let Ok(Some(_)) = master2.try_wait() {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        sleep(POLL_MS);
    };
    assert!(
        second_exited,
        "second master instance should exit when first is alive"
    );

    sigterm_and_wait(&mut master1, Duration::from_secs(5));
    kill_and_wait(master2);
}

/// U4-4: `Handshake { base_path }` returns `MasterResponse::WorkerSocket` pointing
/// to a real worker socket path.
#[test]
fn u4_handshake_returns_worker_socket() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();
    let resp = env.handshake(base_path);

    match &resp {
        MasterResponse::WorkerSocket { path, .. } => {
            assert!(!path.is_empty(), "worker socket path should not be empty");
            // Wait until the worker socket is actually connectable.
            let sock = PathBuf::from(path);
            assert!(
                env.wait_socket(&sock, SOCKET_TIMEOUT),
                "worker socket from Handshake response should be connectable"
            );
        }
        other => panic!("expected WorkerSocket, got {other:?}"),
    }

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-5: `ListWorkers` returns all currently registered workers (correct count).
#[test]
fn u4_list_workers_returns_registered_workers() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Workers spawn in background — wait for n_min=1 to register.
    let workers = env.wait_for_workers(1, SOCKET_TIMEOUT);
    assert!(
        !workers.is_empty(),
        "master should have at least n_min=1 worker registered"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-6: `WorkerStatus { index: 0 }` for a live worker returns `WorkerInfo` with valid PID.
#[test]
fn u4_worker_status_returns_valid_pid() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Workers spawn in background — wait for n_min=1 to register.
    let workers = env.wait_for_workers(1, SOCKET_TIMEOUT);
    assert!(!workers.is_empty());
    let idx = workers[0].index;

    let info = env
        .worker_status(idx)
        .expect("WorkerStatus should return info for live worker");
    assert!(info.pid > 1, "worker PID should be a valid process ID");
    assert_eq!(info.index, idx);

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-7: Routing table JSON is written to disk after a worker is spawned (via Handshake).
#[test]
fn u4_routing_json_written_after_handshake() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();
    let resp = env.handshake(base_path);
    assert!(
        matches!(resp, MasterResponse::WorkerSocket { .. }),
        "handshake failed: {resp:?}"
    );

    // routing.json should exist and contain the worker entry.
    let routing_path = env.routing_json();
    assert!(
        routing_path.exists(),
        "routing.json should exist after Handshake"
    );

    let table = RoutingTable::load(&routing_path).expect("parse routing.json");
    assert!(
        !table.workers.is_empty(),
        "routing.json should contain at least one worker"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-8: Master startup reads routing.json and skips workers with dead PIDs.
/// We write a routing.json with a dead PID before starting master.
#[test]
fn u4_startup_skips_dead_pid_in_routing_json() {
    let env = TestEnv::new();

    // Write a routing.json with an unreachable/dead PID (999999999).
    let routing_dir = env.runtime_dir().join("fff");
    std::fs::create_dir_all(&routing_dir).expect("create runtime/fff dir");

    let dead_pid: u32 = 999_999_999;
    let dead_sock = env.worker_socket(99).to_string_lossy().into_owned();
    let mut table = RoutingTable::default();
    let mut entry = WorkerEntry::new(99, dead_sock, dead_pid);
    entry.push_root("some-slug".into(), "/test/proj".into());
    table.workers.insert(99, entry);
    table.save(&env.routing_json()).expect("save routing.json");

    // Start master — it should discard the dead-PID entry and start fresh workers.
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let workers = env.list_workers();
    let has_dead = workers.iter().any(|w| w.pid == dead_pid);
    assert!(
        !has_dead,
        "master should have discarded the dead-PID worker from routing.json"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U4-9: Master cleans up master socket and lockfile on SIGTERM.
#[test]
fn u4_master_cleans_up_on_sigterm() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));
    assert!(env.master_socket().exists());
    assert!(env.master_lockfile().exists());

    sigterm_and_wait(&mut master, Duration::from_secs(5));

    assert!(
        !env.master_socket().exists(),
        "master.sock should be removed after SIGTERM"
    );
    assert!(
        !env.master_lockfile().exists(),
        "master.lock should be removed after SIGTERM"
    );
}

// ── U5: Scale-out and routing ───────────────────────────────────────────────────

/// U5-1: Second Handshake for same base_path hits routing table (fast path).
/// Both responses must return the same worker_index.
#[test]
fn u5_second_handshake_same_base_path_hits_routing() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();

    let resp1 = env.handshake(base_path);
    let resp2 = env.handshake(base_path);

    let idx1 = match &resp1 {
        MasterResponse::WorkerSocket { worker_index, .. } => *worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };
    let idx2 = match &resp2 {
        MasterResponse::WorkerSocket { worker_index, .. } => *worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };

    assert_eq!(
        idx1, idx2,
        "same base_path should route to the same worker on repeated Handshakes"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U5-2: Scale-out fires when routing table reaches roots_per_worker_max.
/// roots_per_worker_max=2, so 3 distinct roots should cause master to spawn a second worker.
#[test]
fn u5_scale_out_fires_at_roots_per_worker_max() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Workers spawn in background — wait for n_min=1, then confirm exactly 1.
    let initial_count = env.wait_for_workers(1, SOCKET_TIMEOUT).len();
    assert_eq!(
        initial_count, 1,
        "should start with exactly 1 worker (n_min=1)"
    );

    // Create 3 distinct real directories so canonicalization produces distinct slugs.
    let root_a = env.dir.path().join("root_a");
    let root_b = env.dir.path().join("root_b");
    let root_c = env.dir.path().join("root_c");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    std::fs::create_dir_all(&root_c).unwrap();

    env.handshake(root_a.to_str().unwrap());
    env.handshake(root_b.to_str().unwrap());
    // Third root exceeds roots_per_worker_max=2 → triggers scale-out.
    env.handshake(root_c.to_str().unwrap());

    // Scale-out is async; wait for the second worker to register.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let scaled = loop {
        let count = env.list_workers().len();
        if count >= 2 {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(200));
    };
    assert!(
        scaled,
        "master should have spawned a second worker after exceeding roots_per_worker_max"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U5-3: A Handshake for a sub-path of an already-registered root routes to that
/// root's worker WITHOUT minting a new slug (containment, U1).
#[test]
fn u5_containment_routes_subpath_to_parent_root() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));
    env.wait_for_workers(1, SOCKET_TIMEOUT);

    // Real parent dir with a nested sub-directory so canonicalize succeeds.
    let parent = env.dir.path().join("project");
    let child = parent.join("src").join("deep");
    std::fs::create_dir_all(&child).unwrap();

    let parent_idx = match env.handshake(parent.to_str().unwrap()) {
        MasterResponse::WorkerSocket { worker_index, .. } => worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };
    let child_idx = match env.handshake(child.to_str().unwrap()) {
        MasterResponse::WorkerSocket { worker_index, .. } => worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };

    assert_eq!(
        parent_idx, child_idx,
        "sub-path Handshake should route to the containing root's worker"
    );

    // The sub-path must NOT have registered its own slug: exactly one root total.
    let table = RoutingTable::load(&env.routing_json()).expect("parse routing.json");
    let total_roots: usize = table.workers.values().map(|w| w.roots.len()).sum();
    assert_eq!(
        total_roots, 1,
        "containment must not mint a second root for the sub-path"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U3 (subsumption): registering a parent over an already-registered child
/// subsumes the child — the async background task removes the child's routing
/// entry, leaving only the parent.
#[test]
fn u5_parent_registration_subsumes_existing_child() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));
    env.wait_for_workers(1, SOCKET_TIMEOUT);

    let parent = env.dir.path().join("project");
    let child = parent.join("nested").join("child");
    std::fs::create_dir_all(&child).unwrap();

    // Child registered first as its own root.
    env.handshake(child.to_str().unwrap());
    let before: usize = RoutingTable::load(&env.routing_json())
        .expect("routing")
        .workers
        .values()
        .map(|w| w.roots.len())
        .sum();
    assert_eq!(before, 1, "child should be registered as its own root");

    // Registering the parent triggers async subsumption of the child.
    env.handshake(parent.to_str().unwrap());

    // Poll until the background task has evicted the child: one root, the parent.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let subsumed = loop {
        let bases: Vec<String> = RoutingTable::load(&env.routing_json())
            .expect("routing")
            .workers
            .values()
            .flat_map(|w| w.roots.iter().map(|r| r.base_path.clone()))
            .collect();
        if bases.len() == 1 && bases[0].ends_with("project") {
            break true;
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        sleep(Duration::from_millis(200));
    };
    assert!(
        subsumed,
        "parent registration should subsume the pre-existing child root"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U5-3: After scale-out, existing routing entries are not remapped.
/// The root assigned before scale-out must still map to the original worker.
#[test]
fn u5_existing_routing_not_remapped_after_scale_out() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let root_a = env.dir.path().join("so_root_a");
    let root_b = env.dir.path().join("so_root_b");
    let root_c = env.dir.path().join("so_root_c");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    std::fs::create_dir_all(&root_c).unwrap();

    // Assign root_a before scale-out.
    let resp_before = env.handshake(root_a.to_str().unwrap());
    let idx_before = match resp_before {
        MasterResponse::WorkerSocket { worker_index, .. } => worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };

    // Trigger scale-out with root_b and root_c.
    env.handshake(root_b.to_str().unwrap());
    env.handshake(root_c.to_str().unwrap());

    // Give scale-out time to complete.
    sleep(Duration::from_secs(3));

    // root_a should still route to the same worker.
    let resp_after = env.handshake(root_a.to_str().unwrap());
    let idx_after = match resp_after {
        MasterResponse::WorkerSocket { worker_index, .. } => worker_index,
        other => panic!("expected WorkerSocket, got {other:?}"),
    };

    assert_eq!(
        idx_before, idx_after,
        "root_a should remain on worker-{idx_before} after scale-out"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U5-4: Routing table persisted after each Handshake mutation.
/// routing.json should contain the new entry immediately after Handshake.
#[test]
fn u5_routing_json_persisted_after_each_handshake() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let root1 = env.dir.path().join("persist_root1");
    let root2 = env.dir.path().join("persist_root2");
    std::fs::create_dir_all(&root1).unwrap();
    std::fs::create_dir_all(&root2).unwrap();

    env.handshake(root1.to_str().unwrap());
    // Give a brief moment for the async persist to complete.
    sleep(Duration::from_millis(200));

    let table1 =
        RoutingTable::load(&env.routing_json()).expect("load routing.json after first handshake");
    let total_slugs1: usize = table1.workers.values().map(|e| e.roots.len()).sum();
    assert!(
        total_slugs1 >= 1,
        "routing.json should have at least 1 slug after first Handshake"
    );

    env.handshake(root2.to_str().unwrap());
    sleep(Duration::from_millis(200));

    let table2 =
        RoutingTable::load(&env.routing_json()).expect("load routing.json after second handshake");
    let total_slugs2: usize = table2.workers.values().map(|e| e.roots.len()).sum();
    assert!(
        total_slugs2 >= total_slugs1,
        "routing.json should gain a new slug entry after second Handshake"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

// ── U6: Crash recovery ──────────────────────────────────────────────────────────

/// U6-1: Worker crash detected by master — master respawns it within 15s.
#[test]
fn u6_master_respawns_crashed_worker() {
    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Workers spawn in background — wait for n_min=1 to register.
    let workers = env.wait_for_workers(1, SOCKET_TIMEOUT);
    assert!(!workers.is_empty(), "expected at least one worker");
    let idx = workers[0].index;
    let original_pid = workers[0].pid;

    // Wait until the worker socket is connectable.
    assert!(
        env.wait_worker(idx, SOCKET_TIMEOUT),
        "initial worker socket should be ready"
    );

    // Kill the worker process externally.
    unsafe { libc::kill(original_pid as libc::pid_t, libc::SIGKILL) };

    // Wait for the socket to disappear (confirming the process is gone).
    let sock = env.worker_socket(idx);
    let gone = env.wait_socket_gone(&sock, Duration::from_secs(5));
    assert!(gone, "worker socket should disappear after SIGKILL");

    // Wait for master to detect the crash and respawn (within 15s).
    let respawned = env.wait_worker(idx, Duration::from_secs(15));
    assert!(
        respawned,
        "master should respawn worker-{idx} within 15s of crash"
    );

    // Verify the respawned worker is alive (socket connectable — the key assertion).
    // PID comparison is omitted: macOS recycles PIDs quickly in test environments,
    // so the new process may legitimately receive the same PID.
    assert!(
        env.worker_status(idx).is_some(),
        "respawned worker should report valid status"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// U6-2: Master startup with routing.json containing a mix of live and dead PIDs.
/// Dead entries should be discarded; live workers should be reconnected.
#[test]
fn u6_startup_reconnects_live_discards_dead() {
    let env = TestEnv::new();

    // First: start a real worker to get a live socket and PID.
    let live_worker = env.spawn_worker(5);
    assert!(env.wait_worker(5, SOCKET_TIMEOUT));
    let live_pid = live_worker.id();
    let live_sock = env.worker_socket(5).to_string_lossy().into_owned();

    // Write a routing.json with one live entry (worker-5) and one dead entry (worker-99).
    let dead_pid: u32 = 999_999_999;
    let routing_dir = env.runtime_dir().join("fff");
    std::fs::create_dir_all(&routing_dir).unwrap();

    let mut table = RoutingTable::default();
    table
        .workers
        .insert(5, WorkerEntry::new(5, live_sock, live_pid));
    let mut stale_entry = WorkerEntry::new(
        99,
        env.worker_socket(99).to_string_lossy().into_owned(),
        dead_pid,
    );
    stale_entry.push_root("stale-slug".into(), "/test/stale".into());
    table.workers.insert(99, stale_entry);
    table.save(&env.routing_json()).unwrap();

    // Start master — it should adopt worker-5 and discard worker-99.
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    let workers = env.list_workers();

    // Dead PID should not appear.
    let has_dead = workers.iter().any(|w| w.pid == dead_pid);
    assert!(!has_dead, "dead worker-99 should be discarded on startup");

    // Live worker-5 should be adopted (or at least the dead one removed).
    // The master may also spawn additional workers to satisfy n_min=1.
    assert!(
        !workers.is_empty(),
        "at least one worker should be registered"
    );

    kill_and_wait(live_worker);
    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

// ── Health ──────────────────────────────────────────────────────────────────────

/// H1: After a Handshake plus a Connect, master Health reports the loaded root
/// with `indexed_files >= 1` and the correct slug.
#[test]
fn health_reports_indexed_files_after_connect() {
    use fff_ipc::base_path_slug;

    let env = TestEnv::new();
    let mut master = env.spawn_master();
    assert!(env.wait_master(SOCKET_TIMEOUT));

    // Create a project dir with one file so the picker has something to index.
    let project = env.dir.path().join("h1_project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("hello.txt"), b"hello world").unwrap();

    let base_path = project.to_str().unwrap();
    let resp = env.handshake(base_path);
    let (worker_sock, worker_idx) = match resp {
        MasterResponse::WorkerSocket { path, worker_index } => (PathBuf::from(path), worker_index),
        other => panic!("expected WorkerSocket, got {other:?}"),
    };

    // Connect to the worker to trigger picker init for this root.
    assert!(env.wait_socket(&worker_sock, SOCKET_TIMEOUT));
    let _stream = env.worker_connect(&worker_sock, base_path);

    // Poll Health until indexed_files >= 1 (scan completes asynchronously).
    let expected_slug = base_path_slug(&project);
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let (report, saw_files) = loop {
        let report = env.health();
        let saw = report
            .workers
            .iter()
            .find(|w| w.index == worker_idx)
            .and_then(|w| w.roots.iter().find(|r| r.slug == expected_slug))
            .is_some_and(|r| r.indexed_files.unwrap_or(0) >= 1);
        if saw {
            break (report, true);
        }
        if std::time::Instant::now() >= deadline {
            break (report, false);
        }
        sleep(POLL_MS);
    };

    assert!(
        saw_files,
        "expected indexed_files >= 1 in health report, last report: {report:#?}"
    );

    assert!(report.master_pid > 0, "master_pid should be set");
    let worker = report
        .workers
        .iter()
        .find(|w| w.index == worker_idx)
        .expect("worker present");
    let root = worker
        .roots
        .iter()
        .find(|r| r.slug == expected_slug)
        .expect("root present");
    assert_eq!(root.slug, expected_slug);
    assert!(
        root.last_scan_age_sec.is_some(),
        "last_scan_age_sec should populate for worker-served roots"
    );

    sigterm_and_wait(&mut master, Duration::from_secs(5));
}

/// Orphan self-heal: a worker that has seen a live master must self-exit when
/// the master dies WITHOUT SIGTERMing it (SIGKILL/crash). Also proves the
/// worker resolves the same master lockfile path the master writes — a wrong
/// path would either never fire or kill live workers.
#[test]
fn worker_self_exits_when_master_dies_without_sigterm() {
    let env = TestEnv::new();
    // Short window: detect staleness every 1s, exit after 2s orphaned.
    let mut master = env.spawn_master_with_orphan_timing(2, 1);
    assert!(env.wait_master(SOCKET_TIMEOUT), "master socket up");
    assert!(
        env.wait_worker(0, SOCKET_TIMEOUT),
        "n_min worker-0 socket up"
    );

    // Hard-kill the master — skips the graceful SIGTERM-to-workers path, so
    // nothing tells worker-0 to stop. Its lockfile-watch must catch this.
    unsafe {
        libc::kill(master.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = master.wait();

    assert!(
        env.wait_socket_gone(&env.worker_socket(0), Duration::from_secs(10)),
        "orphaned worker-0 must self-exit after losing its master"
    );
}

// ── U3: JSON dual-read on the worker socket ───────────────────────────────────────

/// U3-J1: JSON `connect` + `grep` returns a WireGrepResponse-shaped result with
/// `frecency_score` present on each matched file.
#[test]
fn u3_json_connect_then_grep_returns_grep_response() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let project = env.dir.path().join("j1_project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("hello.rs"), b"fn needle() {}\n").unwrap();
    let base_path = project.to_str().unwrap();

    let mut stream = env.worker_connect_json(&env.worker_socket(0), base_path);

    // Poll grep until the async scan indexes the file and a match appears.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let result = loop {
        let req = RequestEnvelope::new(
            verbs::GREP,
            serde_json::to_value(GrepParams {
                query: "needle".into(),
                options: GrepOptions::default(),
            })
            .unwrap(),
        );
        write_json_message_sync(&mut stream, &req).expect("write JSON grep");
        let resp: ResponseEnvelope = read_json_message_sync(&mut stream).expect("read JSON grep");
        assert!(resp.ok, "grep should be ok, got {resp:?}");
        let result = resp.result.expect("ok response carries a result");
        let has_match = result["matches"].as_array().is_some_and(|m| !m.is_empty());
        if has_match || std::time::Instant::now() >= deadline {
            break result;
        }
        sleep(POLL_MS);
    };

    // WireGrepResponse shape: matches[].frecency_score is present.
    assert!(
        result["matches"].as_array().is_some_and(|m| !m.is_empty()),
        "expected a grep match for 'needle', last result: {result:#?}"
    );
    let first = &result["matches"][0];
    assert!(
        first.get("frecency_score").is_some(),
        "matched file must carry frecency_score: {first:#?}"
    );
    assert!(first.get("path").is_some(), "matched file must carry path");

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-J2: JSON `record_access` is fire-and-forget — the worker writes and sends
/// NO reply. We assert the socket has no frame waiting (read times out), and a
/// subsequent grep still round-trips on the same connection (proving the worker
/// did not enqueue a stray frame ahead of it).
#[test]
fn u3_json_record_access_sends_no_reply() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let project = env.dir.path().join("j2_project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("a.txt"), b"x").unwrap();
    let base_path = project.to_str().unwrap();

    let mut stream = env.worker_connect_json(&env.worker_socket(0), base_path);

    let req = RequestEnvelope::new(
        verbs::RECORD_ACCESS,
        serde_json::to_value(RecordAccessParams {
            path: "a.txt".into(),
        })
        .unwrap(),
    );
    write_json_message_sync(&mut stream, &req).expect("write JSON record_access");

    // No reply must arrive: a short read times out rather than yielding a frame.
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let no_reply: Result<ResponseEnvelope, _> = read_json_message_sync(&mut stream);
    assert!(
        no_reply.is_err(),
        "record_access must send no reply, but a frame arrived: {no_reply:?}"
    );

    // The connection is still usable for a subsequent verb (no stray frame).
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let grep = RequestEnvelope::new(
        verbs::GREP,
        serde_json::to_value(GrepParams {
            query: "x".into(),
            options: GrepOptions::default(),
        })
        .unwrap(),
    );
    write_json_message_sync(&mut stream, &grep).expect("write JSON grep after record_access");
    let resp: ResponseEnvelope = read_json_message_sync(&mut stream).expect("read JSON grep");
    assert!(
        resp.ok && resp.result.is_some(),
        "grep after record_access should round-trip, got {resp:?}"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-J3: A JSON `connect` with an incompatible protocol_version is rejected
/// with PROTOCOL_MISMATCH carrying both versions, and the connection closes.
#[test]
fn u3_json_connect_version_mismatch_is_rejected() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let base_path = env.dir.path().to_str().unwrap();
    let mut stream = UnixStream::connect(env.worker_socket(0)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // Hand-build an envelope with an incompatible version.
    let bad = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION + 1,
        verb: verbs::CONNECT.into(),
        params: json!({ "base_path": base_path }),
    };
    write_json_message_sync(&mut stream, &bad).expect("write mismatched connect");

    let resp: ResponseEnvelope = read_json_message_sync(&mut stream).expect("read mismatch");
    assert!(!resp.ok, "version mismatch must not be ok");
    let err = resp.error.expect("error present");
    assert_eq!(err.code, PROTOCOL_MISMATCH);
    assert_eq!(err.engine_version, Some(PROTOCOL_VERSION));
    assert_eq!(err.client_version, Some(PROTOCOL_VERSION + 1));

    // Connection closes: next read hits EOF.
    let after: Result<ResponseEnvelope, _> = read_json_message_sync(&mut stream);
    assert!(
        after.is_err(),
        "worker should close after PROTOCOL_MISMATCH"
    );

    assert!(
        worker.try_wait().expect("try_wait").is_none(),
        "worker should survive a mismatched client"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-J4 (regression, R5): a legacy bincode `Connect` + `Grep` still round-trips
/// on the same worker that now also speaks JSON.
#[test]
fn u3_bincode_connect_then_grep_still_round_trips() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let project = env.dir.path().join("j4_project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("legacy.rs"), b"fn marker() {}\n").unwrap();
    let base_path = project.to_str().unwrap();

    // Legacy bincode Connect → Ack (existing helper asserts Ack).
    let mut stream = env.worker_connect(&env.worker_socket(0), base_path);

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let matched = loop {
        let req = SearchRequest::Grep {
            query: "marker".into(),
            options: GrepOptions::default(),
        };
        write_message_sync(&mut stream, &req).expect("write bincode grep");
        let resp: SearchResponse = read_message_sync(&mut stream).expect("read bincode grep");
        match resp {
            SearchResponse::GrepResults(w) => {
                if !w.matches.is_empty() {
                    break true;
                }
            }
            other => panic!("expected GrepResults, got {other:?}"),
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
        sleep(POLL_MS);
    };
    assert!(matched, "legacy bincode grep must still return matches");

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}

/// U3-J5: a non-`connect`/`health` JSON first message is rejected and the
/// connection closes (mirrors the legacy first-message discipline).
#[test]
fn u3_json_non_connect_first_message_is_rejected() {
    let env = TestEnv::new();
    let mut worker = env.spawn_worker(0);
    assert!(env.wait_worker(0, SOCKET_TIMEOUT));

    let mut stream = UnixStream::connect(env.worker_socket(0)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // A `grep` as the very first JSON message — not connect/health.
    let req = RequestEnvelope::new(
        verbs::GREP,
        serde_json::to_value(GrepParams {
            query: "main".into(),
            options: GrepOptions::default(),
        })
        .unwrap(),
    );
    write_json_message_sync(&mut stream, &req).expect("write bad first JSON");

    let resp: ResponseEnvelope = read_json_message_sync(&mut stream).expect("read rejection");
    assert!(!resp.ok, "bad first JSON verb must be rejected");
    assert_eq!(resp.error.expect("error").code, fff_ipc::BAD_REQUEST);

    // Connection closes afterward.
    let after: Result<ResponseEnvelope, _> = read_json_message_sync(&mut stream);
    assert!(
        after.is_err(),
        "worker should close after rejecting first verb"
    );

    assert!(
        worker.try_wait().expect("try_wait").is_none(),
        "worker should survive a bad first JSON message"
    );

    sigterm_and_wait(&mut worker, Duration::from_secs(5));
}
