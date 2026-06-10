//! Per-session connection pool keyed on canonicalized base_path.
//!
//! Each unique base_path gets its own `EngineClient`. Cache misses run the
//! full two-phase Handshake → Connect → Ack sequence; hits return a cheap
//! `Arc` clone. On a stale-connection error the caller invalidates and
//! retries once with a fresh handshake.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fff_ipc::types::{SearchRequest, SearchResponse};

use crate::client::EngineClient;

/// Canonicalize for pool lookup. Mirrors `fff_ipc::paths::base_path_slug`:
/// try `fs::canonicalize`, fall back to the raw `PathBuf`.
pub fn canonicalize_key(base_path: &Path) -> PathBuf {
    std::fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf())
}

type ClientCell = Arc<Mutex<EngineClient>>;

pub struct ConnectionPool {
    clients: Mutex<HashMap<PathBuf, ClientCell>>,
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionPool {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// Seed the pool with an already-connected client (used by main to insert
    /// the default-root client built before the MCP server starts).
    pub fn insert(&self, base_path: &Path, client: EngineClient) {
        let key = canonicalize_key(base_path);
        let mut map = self.clients.lock().expect("pool mutex poisoned");
        map.insert(key, Arc::new(Mutex::new(client)));
    }

    /// Return a cached client for `base_path`, or run a fresh Handshake and
    /// cache the result. Concurrent misses for the same key may produce more
    /// than one Handshake; last-writer-wins on insert.
    pub fn get_or_connect(
        &self,
        base_path: &Path,
    ) -> Result<ClientCell, Box<dyn std::error::Error>> {
        let key = canonicalize_key(base_path);
        if let Some(cell) = self.lookup(&key) {
            return Ok(cell);
        }

        let client = EngineClient::connect(base_path)?;
        let cell = Arc::new(Mutex::new(client));
        let mut map = self.clients.lock().expect("pool mutex poisoned");
        // Last-writer-wins: another miss may have already inserted; that
        // entry's connection is redundant but harmless.
        map.insert(key, cell.clone());
        Ok(cell)
    }

    /// Drop the cached entry for `base_path`. Called after a stale-connection
    /// error so the next `get_or_connect` re-Handshakes.
    pub fn invalidate(&self, base_path: &Path) {
        let key = canonicalize_key(base_path);
        let mut map = self.clients.lock().expect("pool mutex poisoned");
        map.remove(&key);
    }

    /// Run `req` against a cached client, invalidating and retrying once
    /// with a fresh client when the response looks like a connection error.
    pub fn search_with_retry(&self, base_path: &Path, req: &SearchRequest) -> SearchResponse {
        let first = match self.get_or_connect(base_path) {
            Ok(cell) => cell,
            Err(e) => {
                return SearchResponse::Error(format!("fff-engine connect failed: {e}"));
            }
        };

        let first_resp = call_client(&first, req, base_path);
        if !is_connection_error(&first_resp) {
            return first_resp;
        }

        tracing::warn!(
            "pool: stale connection for {} — invalidating and retrying",
            base_path.display()
        );
        self.invalidate(base_path);

        let second = match self.get_or_connect(base_path) {
            Ok(cell) => cell,
            Err(e) => {
                return SearchResponse::Error(format!("fff-engine reconnect failed: {e}"));
            }
        };
        call_client(&second, req, base_path)
    }

    fn lookup(&self, key: &Path) -> Option<ClientCell> {
        let map = self.clients.lock().expect("pool mutex poisoned");
        map.get(key).cloned()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.clients.lock().expect("pool mutex poisoned").len()
    }
}

fn call_client(cell: &ClientCell, req: &SearchRequest, base_path: &Path) -> SearchResponse {
    let mut guard = match cell.lock() {
        Ok(g) => g,
        Err(e) => return SearchResponse::Error(format!("client mutex poisoned: {e}")),
    };
    guard.search_with_recovery(req, base_path)
}

fn is_connection_error(resp: &SearchResponse) -> bool {
    match resp {
        SearchResponse::Error(msg) => {
            let lower = msg.to_ascii_lowercase();
            lower.contains("ipc")
                || lower.contains("broken pipe")
                || lower.contains("connection")
                || lower.contains("unavailable")
                || lower.contains("recovery")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_key_falls_back_when_path_missing() {
        let missing = PathBuf::from("/this/path/does/not/exist/at/all");
        let key = canonicalize_key(&missing);
        assert_eq!(key, missing);
    }

    #[test]
    fn canonicalize_key_resolves_existing_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().canonicalize().expect("canonicalize tempdir");
        let key = canonicalize_key(tmp.path());
        assert_eq!(key, real);
    }

    #[test]
    fn invalidate_drops_only_named_entry() {
        let pool = ConnectionPool::new();
        {
            let mut map = pool.clients.lock().unwrap();
            map.insert(
                PathBuf::from("/repo/a"),
                Arc::new(Mutex::new(stub_client(PathBuf::from("/repo/a")))),
            );
            map.insert(
                PathBuf::from("/repo/b"),
                Arc::new(Mutex::new(stub_client(PathBuf::from("/repo/b")))),
            );
        }
        assert_eq!(pool.len(), 2);

        pool.invalidate(Path::new("/repo/a"));
        assert_eq!(pool.len(), 1);
        assert!(pool.lookup(Path::new("/repo/b")).is_some());
        assert!(pool.lookup(Path::new("/repo/a")).is_none());
    }

    #[test]
    fn insert_caches_per_canonicalized_key() {
        let pool = ConnectionPool::new();
        let tmp = tempfile::tempdir().expect("tempdir");
        let canon = tmp.path().canonicalize().expect("canonicalize");
        let stub = stub_client(canon.clone());
        pool.insert(tmp.path(), stub);

        assert!(pool.lookup(&canon).is_some(), "canonical lookup must hit");
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn is_connection_error_classifies_ipc_messages() {
        assert!(is_connection_error(&SearchResponse::Error(
            "IPC error: broken pipe".into()
        )));
        assert!(is_connection_error(&SearchResponse::Error(
            "fff-engine unavailable after recovery".into()
        )));
        assert!(!is_connection_error(&SearchResponse::Error(
            "no such file".into()
        )));
        assert!(!is_connection_error(&SearchResponse::Ack));
    }

    fn stub_client(base_path: PathBuf) -> EngineClient {
        use std::os::unix::net::UnixStream;
        let (a, _b) = UnixStream::pair().expect("socketpair");
        EngineClient::from_stream(a, base_path)
    }
}
