//! Synchronous IPC client for fff-engine — two-phase connect via master.
//!
//! Speaks the versioned JSON envelope (`fff_ipc::protocol`) on the hot path:
//! handshake, connect, search, and record_access all ride the documented
//! `[u32-LE len][json]` framing. `set_log_level` and `check_health` stay on the
//! legacy bincode path (no JSON verb / read-only probe); the engine dual-reads
//! both encodings, so the mix is transparent on the wire.
//!
//! Phase 1: connect to master socket, send a `handshake` envelope, receive a
//! `HandshakeResult{worker_socket, worker_index}`. A protocol mismatch fails
//! loud here (refuse-on-mismatch) instead of silently garbling.
//! Phase 2: connect to the worker socket, send a `connect` envelope, await ack.
//! All subsequent search traffic uses the direct worker connection.

use std::io::{BufReader, BufWriter};
use std::os::unix::{fs::MetadataExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use fff_ipc::protocol::{
    BasePathParams, FindFilesParams, GetGitStatusParams, GrepParams, HandshakeResult,
    ListDirectoriesParams, ListRecentFilesParams, MultiGrepParams, PROTOCOL_MISMATCH,
    RecordAccessParams, RequestEnvelope, ResponseEnvelope, verbs,
};
use fff_ipc::types::{
    HealthResponse, MasterRequest, MasterResponse, SearchRequest, SearchResponse, WireDirEntry,
    WireGitFile, WireGrepResponse, WireSearchResult,
};
use fff_ipc::{IpcError, lockfile, master_lockfile_path, master_socket_path};
use fff_ipc::{
    read_json_message_sync, read_message_sync, write_json_message_sync, write_message_sync,
};

const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// Read timeout for the Connect→Ack handshake — short so a dead worker is
/// detected fast.
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Read timeout for ongoing queries once connected. Must exceed the engine's
/// cold-start query-readiness wait (`GREP_READINESS_TIMEOUT` = 30s in
/// fff-engine) so a slow initial scan does not make the client preempt a
/// response the engine is deliberately holding while the index warms up.
const QUERY_READ_TIMEOUT: Duration = Duration::from_secs(35);

pub struct EngineClient {
    reader: BufReader<UnixStream>,
    writer: BufWriter<UnixStream>,
    pub(crate) base_path: PathBuf,
}

impl EngineClient {
    /// Connect to the fff-engine for `base_path` via master two-phase handshake.
    ///
    /// If master is not running, spawns `fff-engine --master` first.
    /// Falls back to the legacy singleton path when master spawn fails and a
    /// per-root socket exists (backwards compatibility).
    pub fn connect(base_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        // Ensure master is running; spawn if absent.
        ensure_master_running()?;

        // Phase 1: handshake with master to get the worker socket path.
        let worker_socket = master_handshake(base_path)?;

        // Phase 2: connect to the worker and send Connect{base_path}.
        let stream = wait_and_connect(&worker_socket, SPAWN_TIMEOUT)?;
        // Short timeout for the Connect→Ack handshake so a dead worker is
        // detected fast; raised to QUERY_READ_TIMEOUT once connected.
        stream.set_read_timeout(Some(HANDSHAKE_READ_TIMEOUT))?;
        stream.set_write_timeout(Some(HANDSHAKE_READ_TIMEOUT))?;
        let base_path_str = base_path.to_string_lossy().into_owned();

        let mut writer = BufWriter::new(stream.try_clone()?);
        let mut reader = BufReader::new(stream);

        let connect_env = RequestEnvelope::new(
            verbs::CONNECT,
            serde_json::to_value(BasePathParams {
                base_path: base_path_str.clone(),
            })?,
        );
        write_json_message_sync(&mut writer, &connect_env)?;
        use std::io::Write;
        writer.flush().map_err(IpcError::Io)?;

        let connect_resp: ResponseEnvelope = read_json_message_sync(&mut reader)?;
        if !connect_resp.ok {
            let msg = connect_resp
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "worker Connect rejected".into());
            return Err(format!("worker Connect rejected: {msg}").into());
        }

        // Handshake done — raise the read timeout for ongoing queries. The
        // engine may hold a response for up to its cold-start readiness wait
        // (30s) while a fresh index finishes scanning; the short handshake
        // timeout would otherwise preempt a legitimately slow first query.
        reader
            .get_ref()
            .set_read_timeout(Some(QUERY_READ_TIMEOUT))?;

        Ok(Self {
            reader,
            writer,
            base_path: base_path.to_path_buf(),
        })
    }

    /// The base path this client is connected to.
    #[allow(dead_code)]
    pub fn base_path(&self) -> &std::path::Path {
        &self.base_path
    }

    /// Ensure `fff-engine --master` is running, spawning it if absent.
    /// Lets callers (re)attach to master mode without a full two-phase connect.
    pub fn ensure_master() -> Result<(), Box<dyn std::error::Error>> {
        ensure_master_running()
    }

    /// Construct from an arbitrary stream — pool tests only.
    #[cfg(test)]
    pub(crate) fn from_stream(stream: UnixStream, base_path: PathBuf) -> Self {
        let writer = BufWriter::new(stream.try_clone().expect("clone stream"));
        Self {
            reader: BufReader::new(stream),
            writer,
            base_path,
        }
    }

    /// Connect directly to a legacy per-root singleton engine.
    ///
    /// Uses `fff_ipc::socket_path(base_path)` — the singleton's well-known socket.
    /// No `Connect` handshake is sent; the legacy engine speaks search requests
    /// directly after the TCP-like accept. Use as a fallback when the master is
    /// unavailable (R2 resilience).
    pub fn connect_legacy(base_path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let sock = fff_ipc::socket_path(base_path);
        let stream = UnixStream::connect(&sock)
            .map_err(|e| format!("legacy per-root socket {}: {e}", sock.display()))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            writer: BufWriter::new(stream.try_clone()?),
            reader: BufReader::new(stream),
            base_path: base_path.to_path_buf(),
        })
    }

    /// Re-run the two-phase handshake and return a fresh client. Used by recovery.
    #[allow(dead_code)]
    pub fn reconnect(&self) -> Result<Self, Box<dyn std::error::Error>> {
        Self::connect(&self.base_path)
    }

    /// Send a search request, with transparent crash recovery.
    pub fn search_with_recovery(
        &mut self,
        req: &SearchRequest,
        base_path: &Path,
    ) -> SearchResponse {
        match self.search(req) {
            Ok(resp) => return resp,
            Err(e) => tracing::warn!("worker socket error: {e}; attempting recovery"),
        }

        // Re-run two-phase handshake to get a fresh worker connection.
        match crate::recovery::respawn(base_path) {
            Ok(new_client) => {
                *self = new_client;
                match self.search(req) {
                    Ok(resp) => resp,
                    Err(e) => {
                        SearchResponse::Error(format!("fff-engine unavailable after recovery: {e}"))
                    }
                }
            }
            Err(e) => SearchResponse::Error(format!("fff-engine recovery failed: {e}")),
        }
    }

    /// Low-level send with no retry. Encodes the request as a versioned JSON
    /// envelope and decodes the JSON response back into the existing
    /// `SearchResponse` shape.
    pub fn search(&mut self, req: &SearchRequest) -> Result<SearchResponse, IpcError> {
        let env = searchrequest_to_envelope(req)?;
        write_json_message_sync(&mut self.writer, &env)?;
        use std::io::Write;
        self.writer.flush().map_err(IpcError::Io)?;
        let resp: ResponseEnvelope = read_json_message_sync(&mut self.reader)?;
        Ok(responseenvelope_to_searchresponse(&env.verb, resp))
    }

    /// Hot-reload the daemon's log filter. No JSON verb exists for SetLogLevel
    /// (operator path, not a memory-kit verb), so this stays on legacy bincode;
    /// the engine dual-reads it.
    pub fn set_log_level(&mut self, level: &str) -> Result<SearchResponse, IpcError> {
        write_message_sync(
            &mut self.writer,
            &SearchRequest::SetLogLevel {
                level: level.to_owned(),
            },
        )?;
        use std::io::Write;
        self.writer.flush().map_err(IpcError::Io)?;
        read_message_sync(&mut self.reader)
    }

    /// Fire-and-forget frecency write. Writes and flushes a JSON `record_access`
    /// envelope without reading a reply — the engine sends none for it.
    pub fn record_access(&mut self, path: &str) {
        if let Ok(params) = serde_json::to_value(RecordAccessParams {
            path: path.to_owned(),
        }) {
            let env = RequestEnvelope::new(verbs::RECORD_ACCESS, params);
            let _ = write_json_message_sync(&mut self.writer, &env);
        }
        let _ = {
            use std::io::Write;
            self.writer.flush()
        };
    }

    /// Check daemon health without triggering root initialisation.
    /// Uses MasterRequest::RouteInfo (read-only) instead of a full two-phase connect.
    // Stays on legacy bincode this increment: RouteInfo is a cheap liveness probe
    // that must not trigger root init; the JSON `health` verb fans out to workers
    // and is heavier. Migration deferred.
    pub fn check_health(base_path: &Path) -> HealthStatus {
        use std::io::{BufReader, BufWriter};
        let master = master_socket_path();
        match UnixStream::connect(&master) {
            Ok(stream) if socket_owned_by_us(&master) => {
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut writer = match stream.try_clone().map(BufWriter::new) {
                    Ok(w) => w,
                    Err(_) => return HealthStatus::ConnRefused("clone failed".into()),
                };
                let mut reader = BufReader::new(stream);
                let req = MasterRequest::RouteInfo {
                    base_path: base_path.to_string_lossy().into(),
                };
                use std::io::Write;
                if write_message_sync(&mut writer, &req).is_err() || writer.flush().is_err() {
                    return HealthStatus::ConnRefused("send failed".into());
                }
                let resp: Result<MasterResponse, _> = read_message_sync(&mut reader);
                match resp {
                    Ok(MasterResponse::WorkerInfo(_) | MasterResponse::Error(_)) => {
                        HealthStatus::Ok
                    }
                    _ => HealthStatus::ConnRefused("unexpected response".into()),
                }
            }
            _ => {
                // Master not running — fall back to legacy per-root socket.
                let sock = fff_ipc::socket_path(base_path);
                if !sock.exists() {
                    HealthStatus::NotStarted(master)
                } else {
                    match UnixStream::connect(&sock) {
                        Ok(_) => HealthStatus::Ok,
                        Err(e) => HealthStatus::ConnRefused(e.to_string()),
                    }
                }
            }
        }
    }
}

pub enum HealthStatus {
    Ok,
    NotStarted(std::path::PathBuf),
    ConnRefused(String),
}

// Client-side inverse of the engine's `envelope_to_request` (worker.rs): map a
// SearchRequest to its verb + params envelope. Reuses the fff-ipc param structs
// so field names never drift from the documented contract.
fn searchrequest_to_envelope(req: &SearchRequest) -> Result<RequestEnvelope, IpcError> {
    let to_env = |verb: &str, params: serde_json::Result<serde_json::Value>| {
        params
            .map(|p| RequestEnvelope::new(verb, p))
            .map_err(IpcError::JsonEncode)
    };
    match req {
        SearchRequest::Grep { query, options } => to_env(
            verbs::GREP,
            serde_json::to_value(GrepParams {
                query: query.clone(),
                options: options.clone(),
            }),
        ),
        SearchRequest::FindFiles { query, options } => to_env(
            verbs::FIND_FILES,
            serde_json::to_value(FindFilesParams {
                query: query.clone(),
                options: options.clone(),
            }),
        ),
        SearchRequest::MultiGrep {
            patterns,
            constraints,
            options,
        } => to_env(
            verbs::MULTI_GREP,
            serde_json::to_value(MultiGrepParams {
                patterns: patterns.clone(),
                constraints: constraints.clone(),
                options: options.clone(),
            }),
        ),
        SearchRequest::ListRecentFiles { limit, dirty_only } => to_env(
            verbs::LIST_RECENT_FILES,
            serde_json::to_value(ListRecentFilesParams {
                limit: *limit,
                dirty_only: *dirty_only,
            }),
        ),
        SearchRequest::GetGitStatus { include_clean } => to_env(
            verbs::GET_GIT_STATUS,
            serde_json::to_value(GetGitStatusParams {
                include_clean: *include_clean,
            }),
        ),
        SearchRequest::ListDirectories { limit } => to_env(
            verbs::LIST_DIRECTORIES,
            serde_json::to_value(ListDirectoriesParams { limit: *limit }),
        ),
        SearchRequest::Health => Ok(RequestEnvelope::new(verbs::HEALTH, serde_json::json!({}))),
        // RecordAccess / SetLogLevel / Connect / DropRoot are not routed through
        // search(): they have dedicated paths (fire-and-forget JSON, legacy
        // bincode, the connect handshake) or no client-side caller.
        other => Err(IpcError::Protocol(format!(
            "request not supported on the JSON search path: {other:?}"
        ))),
    }
}

// Client-side inverse of the engine's `searchresponse_to_json_value` (server.rs):
// decode a ResponseEnvelope back into the existing SearchResponse, parsing the
// JSON `result` into the verb's Wire* type. !ok → SearchResponse::Error so the
// existing error channel is preserved. The verb selects which Wire* to expect.
fn responseenvelope_to_searchresponse(verb: &str, resp: ResponseEnvelope) -> SearchResponse {
    if !resp.ok {
        let msg = resp
            .error
            .map(|e| {
                if e.code == PROTOCOL_MISMATCH {
                    format!("{}: {}", PROTOCOL_MISMATCH, e.message)
                } else {
                    e.message
                }
            })
            .unwrap_or_else(|| "engine returned an error without a message".into());
        return SearchResponse::Error(msg);
    }
    let value = match resp.result {
        Some(v) => v,
        None => return SearchResponse::Error("engine ok response missing result".into()),
    };
    let parse_err = |e: serde_json::Error| SearchResponse::Error(format!("malformed result: {e}"));
    match verb {
        verbs::GREP | verbs::MULTI_GREP => {
            match serde_json::from_value::<WireGrepResponse>(value) {
                Ok(w) => SearchResponse::GrepResults(w),
                Err(e) => parse_err(e),
            }
        }
        verbs::FIND_FILES => match serde_json::from_value::<Vec<WireSearchResult>>(value) {
            Ok(v) => SearchResponse::SearchResults(v),
            Err(e) => parse_err(e),
        },
        verbs::LIST_RECENT_FILES => match serde_json::from_value::<Vec<WireSearchResult>>(value) {
            Ok(v) => SearchResponse::RecentFiles(v),
            Err(e) => parse_err(e),
        },
        verbs::GET_GIT_STATUS => match serde_json::from_value::<Vec<WireGitFile>>(value) {
            Ok(v) => SearchResponse::GitStatus(v),
            Err(e) => parse_err(e),
        },
        verbs::LIST_DIRECTORIES => match serde_json::from_value::<Vec<WireDirEntry>>(value) {
            Ok(v) => SearchResponse::Directories(v),
            Err(e) => parse_err(e),
        },
        verbs::HEALTH => match serde_json::from_value::<HealthResponse>(value) {
            Ok(h) => SearchResponse::Health(h),
            Err(e) => parse_err(e),
        },
        other => SearchResponse::Error(format!("no result decoder for verb '{other}'")),
    }
}

/// Send a JSON `handshake` envelope to the master and return the worker socket
/// path. A protocol-version mismatch fails loud (refuse-on-mismatch).
fn master_handshake(base_path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let master = master_socket_path();
    let stream = UnixStream::connect(&master)
        .map_err(|e| format!("cannot connect to master socket {}: {e}", master.display()))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let mut writer = BufWriter::new(stream.try_clone()?);
    let mut reader = BufReader::new(stream);

    let env = RequestEnvelope::new(
        verbs::HANDSHAKE,
        serde_json::to_value(BasePathParams {
            base_path: base_path.to_string_lossy().into_owned(),
        })?,
    );
    write_json_message_sync(&mut writer, &env)?;
    use std::io::Write;
    writer.flush().map_err(IpcError::Io)?;

    let resp: ResponseEnvelope = read_json_message_sync(&mut reader)?;
    let result = handshake_result_from_envelope(resp)?;

    // Validate the returned path is under the expected workers/ directory.
    let expected = fff_ipc::worker_socket_path(result.worker_index);
    let actual = PathBuf::from(&result.worker_socket);
    if actual != expected {
        return Err(format!(
            "master returned unexpected worker socket path: {:?} (expected {expected:?})",
            result.worker_socket
        )
        .into());
    }
    Ok(actual)
}

/// Parse a master handshake `ResponseEnvelope` into a `HandshakeResult`. A
/// `PROTOCOL_MISMATCH` error (or version skew) fails loud rather than hanging or
/// garbling — this is the refuse-on-mismatch contract (R3/KTD4).
fn handshake_result_from_envelope(
    resp: ResponseEnvelope,
) -> Result<HandshakeResult, Box<dyn std::error::Error>> {
    if !resp.ok {
        if let Some(err) = resp.error {
            if err.code == PROTOCOL_MISMATCH {
                return Err(format!(
                    "fff-engine protocol mismatch (engine {:?}, client {:?}): {}",
                    err.engine_version, err.client_version, err.message
                )
                .into());
            }
            return Err(format!("master handshake error [{}]: {}", err.code, err.message).into());
        }
        return Err("master handshake failed without an error body".into());
    }
    let value = resp
        .result
        .ok_or("master handshake ok response missing result")?;
    serde_json::from_value(value).map_err(|e| format!("malformed handshake result: {e}").into())
}

/// Ensure `fff-engine --master` is running, spawning it if absent.
/// Uses an O_CREAT|O_EXCL race so only one spawner wins.
fn ensure_master_running() -> Result<(), Box<dyn std::error::Error>> {
    let master = master_socket_path();

    // Fast path: socket exists, is owned by us, and accepts connections.
    if master.exists() && socket_owned_by_us(&master) && UnixStream::connect(&master).is_ok() {
        return Ok(());
    }

    let lockfile = master_lockfile_path();

    // Ensure parent dir exists before checking or creating the lockfile.
    if let Some(parent) = lockfile.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // A live master holds the lockfile (slow start or another spawner in
    // progress) — just wait. A stale lockfile means the previous master died
    // uncleanly; remove it so our O_EXCL acquire below can win instead of
    // failing and waiting forever for a socket that will never appear.
    if lockfile.exists() {
        if lockfile::is_stale(&lockfile) {
            let _ = std::fs::remove_file(&lockfile);
        } else {
            return fff_ipc::wait_for_socket(&master, SPAWN_TIMEOUT).map_err(Into::into);
        }
    }

    // Race to spawn master: O_CREAT|O_EXCL via create_new.
    use std::fs::OpenOptions;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lockfile)
    {
        Ok(_) => {
            // We won. Write our PID so concurrent losers see a live holder and
            // call wait_for_socket instead of also trying to spawn.
            let _ = std::fs::write(&lockfile, format!("{}\n", std::process::id()));
        }
        Err(_) => {
            // Someone else is spawning; wait for the socket.
            return fff_ipc::wait_for_socket(&master, SPAWN_TIMEOUT).map_err(Into::into);
        }
    }

    let engine_bin = find_engine_bin();
    if let Some(parent) = master.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Remove our temp lockfile BEFORE spawning so the master process does not
    // race with us: if the master starts before we remove the lockfile it sees
    // our (live) PID and exits thinking another master is already running.
    // Any concurrent fff-mcp that races into the window where no lockfile
    // exists will simply lose the O_CREAT|O_EXCL to the real master once it
    // creates its own lockfile, then fall through to wait_for_socket.
    let _ = std::fs::remove_file(&lockfile);

    let spawn_result = Command::new(&engine_bin)
        .arg("--master")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    match spawn_result {
        Ok(child) => tracing::info!("spawned fff-engine --master pid={}", child.id()),
        Err(e) => {
            return Err(format!(
                "failed to start fff-engine --master ({}): {e}",
                engine_bin.display()
            )
            .into());
        }
    };

    let result = fff_ipc::wait_for_socket(&master, SPAWN_TIMEOUT).map_err(Into::into);
    if result.is_err() {
        tracing::warn!(
            "timed out waiting for fff-engine --master socket — \
             binary may have crashed on startup"
        );
    }
    result
}

/// Wait for `path` to accept connections, then connect. Delegates to the
/// canonical fff_ipc::wait_for_socket (polls UnixStream::connect, not path.exists).
fn wait_and_connect(
    path: &Path,
    timeout: Duration,
) -> Result<UnixStream, Box<dyn std::error::Error>> {
    fff_ipc::wait_for_socket(path, timeout)
        .map_err(|e| e.into())
        .and_then(|()| {
            UnixStream::connect(path).map_err(|e| {
                format!("failed to connect to worker socket {}: {e}", path.display()).into()
            })
        })
}

/// Returns true when `path` is owned by the current user.
/// Prevents a rogue process at the master socket path from being trusted.
fn socket_owned_by_us(path: &Path) -> bool {
    let current_uid = unsafe { libc::getuid() };
    std::fs::metadata(path)
        .map(|m| m.uid() == current_uid)
        .unwrap_or(false)
}

fn find_engine_bin() -> PathBuf {
    // Try sibling of current exe, then parent of that dir (handles test binaries
    // living in target/debug/deps/ while the engine is in target/debug/).
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(2) {
            let candidate = ancestor.join("fff-engine");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from("fff-engine")
}

#[cfg(test)]
mod tests {
    use super::*;
    use fff_ipc::protocol::{PROTOCOL_VERSION, ResponseError};
    use fff_ipc::types::{FindOptions, GrepOptions};

    fn find_req() -> SearchRequest {
        SearchRequest::FindFiles {
            query: "q".into(),
            options: FindOptions::default(),
        }
    }

    // Stand-in for the engine's JSON reply on a worker socket: read one request
    // envelope, then write the given ok response envelope. Mirrors the wire the
    // migrated client now speaks so the timeout-discipline tests stay valid.
    fn serve_one_json(server_sock: &mut UnixStream, delay: Duration, result: serde_json::Value) {
        let _req: RequestEnvelope = read_json_message_sync(server_sock).unwrap();
        std::thread::sleep(delay);
        let env = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            result: Some(result),
            error: None,
        };
        let _ = write_json_message_sync(server_sock, &env);
    }

    // A response that arrives within the read timeout is received — the property
    // QUERY_READ_TIMEOUT relies on so a slow cold-start query (engine holds the
    // reply up to ~30s while the index warms) is not preempted by the client.
    #[test]
    fn search_receives_response_delayed_under_read_timeout() {
        let (client_sock, mut server_sock) = UnixStream::pair().unwrap();
        client_sock
            .set_read_timeout(Some(Duration::from_millis(800)))
            .unwrap();

        let server = std::thread::spawn(move || {
            serve_one_json(
                &mut server_sock,
                Duration::from_millis(150),
                serde_json::json!([]),
            );
        });

        let mut client = EngineClient::from_stream(client_sock, PathBuf::from("/x"));
        let resp = client.search(&find_req());
        server.join().unwrap();
        assert!(
            matches!(resp, Ok(SearchResponse::SearchResults(_))),
            "delayed-but-in-window response must arrive, got {resp:?}"
        );
    }

    // A response slower than the read timeout errors (the old no-timeout path
    // hung here forever; a bounded timeout lets search_with_recovery react).
    #[test]
    fn search_errors_when_response_exceeds_read_timeout() {
        let (client_sock, mut server_sock) = UnixStream::pair().unwrap();
        client_sock
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();

        let server = std::thread::spawn(move || {
            serve_one_json(
                &mut server_sock,
                Duration::from_millis(300),
                serde_json::json!([]),
            );
        });

        let mut client = EngineClient::from_stream(client_sock, PathBuf::from("/x"));
        let resp = client.search(&find_req());
        let _ = server.join();
        assert!(
            resp.is_err(),
            "response slower than the read timeout must error"
        );
    }

    #[test]
    fn query_read_timeout_exceeds_engine_cold_start_wait() {
        // fff-engine gates queries on GREP_READINESS_TIMEOUT (30s). The client
        // query read timeout must exceed it or slow cold-start queries fail.
        assert!(QUERY_READ_TIMEOUT >= Duration::from_secs(30));
    }

    // find_files: an ok envelope carrying a Vec<WireSearchResult> decodes to the
    // SearchResults variant — the typed result the tool surface expects.
    #[test]
    fn find_files_response_decodes_to_search_results() {
        let result = serde_json::to_value(vec![WireSearchResult {
            path: "src/main.rs".into(),
            score: 100,
            git_status: Some(0),
            frecency_score: 100,
        }])
        .unwrap();
        let env = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            result: Some(result),
            error: None,
        };
        match responseenvelope_to_searchresponse(verbs::FIND_FILES, env) {
            SearchResponse::SearchResults(v) => {
                assert_eq!(v.len(), 1);
                assert_eq!(v[0].path, "src/main.rs");
            }
            other => panic!("expected SearchResults, got {other:?}"),
        }
    }

    // grep: an ok envelope carrying a WireGrepResponse decodes to GrepResults.
    #[test]
    fn grep_response_decodes_to_grep_results() {
        let result = serde_json::to_value(WireGrepResponse {
            matches: vec![],
            total_files_searched: 7,
            total_files: 9,
            files_with_matches: 0,
            next_file_offset: 0,
            regex_fallback_error: None,
        })
        .unwrap();
        let env = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            result: Some(result),
            error: None,
        };
        match responseenvelope_to_searchresponse(verbs::GREP, env) {
            SearchResponse::GrepResults(w) => assert_eq!(w.total_files, 9),
            other => panic!("expected GrepResults, got {other:?}"),
        }
    }

    // The grep request maps to the documented verb + GrepParams shape.
    #[test]
    fn grep_request_maps_to_grep_verb_and_params() {
        let req = SearchRequest::Grep {
            query: "needle".into(),
            options: GrepOptions::default(),
        };
        let env = searchrequest_to_envelope(&req).unwrap();
        assert_eq!(env.verb, verbs::GREP);
        let params: GrepParams = serde_json::from_value(env.params).unwrap();
        assert_eq!(params.query, "needle");
    }

    // A version-incompatible engine surfaces a loud PROTOCOL_MISMATCH error from
    // the handshake decode rather than hanging or garbling (R3/KTD4).
    #[test]
    fn handshake_mismatch_fails_loud() {
        let env = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION + 1,
            ok: false,
            result: None,
            error: Some(ResponseError {
                code: PROTOCOL_MISMATCH.to_string(),
                message: "engine speaks 2, client sent 1".into(),
                engine_version: Some(PROTOCOL_VERSION + 1),
                client_version: Some(PROTOCOL_VERSION),
            }),
        };
        let err = handshake_result_from_envelope(env).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("protocol mismatch"),
            "mismatch must be explicit, got: {msg}"
        );
    }
}
