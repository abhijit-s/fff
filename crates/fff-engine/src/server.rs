use std::path::PathBuf;
use std::sync::Arc;

use fff_ipc::types::SearchRequest;
use fff_ipc::{read_message, write_message};
#[cfg(unix)]
use tokio::net::UnixListener;

use crate::state::EngineState;

/// Bind a UnixListener at `socket_path` and accept connections until SIGTERM/SIGINT.
#[cfg(unix)]
pub async fn run(
    state: Arc<EngineState>,
    socket_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove any stale socket left by a previous crash.
    let _ = std::fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!("fff-engine listening on {}", socket_path.display());

    let shutdown = async {
        // Wait on whichever of SIGINT (Ctrl-C) or SIGTERM (fffctl stop, init,
        // brew services) arrives first. Returning from this future drops the
        // lockfile guard cleanly so no stale lockfile is left behind.
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received"),
            _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        }
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => {
                        let state_clone = Arc::clone(&state);
                        tokio::spawn(handle_connection(stream, state_clone));
                    }
                    Err(e) => {
                        tracing::error!("Accept error: {e}");
                    }
                }
            }
            _ = &mut shutdown => {
                tracing::info!("Shutdown signal received");
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[cfg(not(unix))]
pub async fn run(
    _state: Arc<EngineState>,
    _socket_path: PathBuf,
) -> Result<(), Box<dyn std::error::Error>> {
    Err("fff-engine singleton mode is not supported on this platform".into())
}

#[cfg(unix)]
async fn handle_connection(stream: tokio::net::UnixStream, state: Arc<EngineState>) {
    let (mut read_half, mut write_half) = tokio::io::split(stream);

    loop {
        let request: SearchRequest = match read_message(&mut read_half).await {
            Ok(req) => req,
            Err(_) => break, // EOF or broken pipe — client disconnected
        };

        match request {
            SearchRequest::RecordAccess { path } => {
                let base_path = state.base_path.clone();
                let frecency = state.shared_frecency.clone();
                tokio::task::spawn_blocking(move || {
                    let abs_path = if std::path::Path::new(&path).is_absolute() {
                        std::path::PathBuf::from(&path)
                    } else {
                        base_path.join(&path)
                    };
                    if let Ok(guard) = frecency.read()
                        && let Some(tracker) = guard.as_ref()
                        && let Err(e) = tracker.track_access(&abs_path)
                    {
                        tracing::warn!(?abs_path, "RecordAccess failed: {e}");
                    }
                });
                // No response — fire-and-forget
            }
            SearchRequest::SetLogLevel { level } => {
                let response = match crate::set_log_level(&level) {
                    Ok(()) => {
                        tracing::info!("Log level changed to {level:?}");
                        fff_ipc::types::SearchResponse::Ack
                    }
                    Err(e) => fff_ipc::types::SearchResponse::Error(e),
                };
                if write_message(&mut write_half, &response).await.is_err() {
                    break;
                }
            }
            SearchRequest::DropRoot { .. } => {
                // Singleton mode has no multi-root map to subsume — no-op Ack.
                if write_message(&mut write_half, &fff_ipc::types::SearchResponse::Ack)
                    .await
                    .is_err()
                {
                    break;
                }
            }
            req => {
                let response = dispatch_request(&state, req).await;
                if write_message(&mut write_half, &response).await.is_err() {
                    break;
                }
            }
        }
    }
}

pub(crate) async fn dispatch_request(
    state: &EngineState,
    req: SearchRequest,
) -> fff_ipc::types::SearchResponse {
    use crate::handlers::{
        handle_find_files, handle_get_git_status, handle_grep, handle_list_directories,
        handle_list_recent_files, handle_multi_grep,
    };
    use std::time::Instant;

    // Stamp the root's idle signal on every served request — not just at
    // Connect. A pooled connection reuses one connect-time state Arc for the
    // process life, so without this a live root's last_access never advances
    // and the reaper evicts it out from under the connection at the TTL.
    state.touch_last_access();

    let start = Instant::now();

    let (label, response) = match req {
        SearchRequest::Grep { query, options } => {
            let label = format!("grep({:?})", query);
            (label, handle_grep(state, query, options).await)
        }
        SearchRequest::FindFiles { query, options } => {
            let label = format!("find_files({:?})", query);
            (label, handle_find_files(state, query, options).await)
        }
        SearchRequest::MultiGrep {
            patterns,
            constraints,
            options,
        } => {
            let label = format!("multi_grep({:?})", patterns);
            (
                label,
                handle_multi_grep(state, patterns, constraints, options).await,
            )
        }
        SearchRequest::ListRecentFiles { limit, dirty_only } => {
            let label = format!("list_recent_files(limit={limit}, dirty_only={dirty_only})");
            (
                label,
                handle_list_recent_files(state, limit, dirty_only).await,
            )
        }
        SearchRequest::GetGitStatus { include_clean } => {
            let label = format!("get_git_status(include_clean={include_clean})");
            (label, handle_get_git_status(state, include_clean).await)
        }
        SearchRequest::ListDirectories { limit } => {
            let label = format!("list_directories(limit={limit})");
            (label, handle_list_directories(state, limit).await)
        }
        SearchRequest::RecordAccess { .. }
        | SearchRequest::SetLogLevel { .. }
        | SearchRequest::DropRoot { .. } => {
            unreachable!("handled before dispatch")
        }
        SearchRequest::Connect { .. } => {
            // Connect is only valid as the first message on a worker socket.
            // The singleton server does not support the worker protocol.
            (
                "connect(rejected)".to_string(),
                fff_ipc::types::SearchResponse::Error(
                    "Connect is not supported in singleton mode".into(),
                ),
            )
        }
        SearchRequest::Health => {
            // Health is only valid as the first message on a worker socket
            // (handled in worker.rs). The singleton server reports per-root
            // health for its single loaded root.
            let resp = crate::handlers::handle_health(state).await;
            ("health".to_string(), resp)
        }
    };

    let elapsed = start.elapsed();
    let result_count = match &response {
        fff_ipc::types::SearchResponse::GrepResults(r) => {
            r.matches.iter().map(|f| f.matches.len()).sum::<usize>()
        }
        fff_ipc::types::SearchResponse::SearchResults(r)
        | fff_ipc::types::SearchResponse::RecentFiles(r) => r.len(),
        fff_ipc::types::SearchResponse::GitStatus(r) => r.len(),
        fff_ipc::types::SearchResponse::Directories(r) => r.len(),
        fff_ipc::types::SearchResponse::Health(r) => r.roots.len(),
        fff_ipc::types::SearchResponse::Error(_) | fff_ipc::types::SearchResponse::Ack => 0,
    };
    tracing::info!("request: {label}");
    tracing::debug!("{label} → {result_count} results in {elapsed:.1?}");

    response
}

// Convert a dispatched SearchResponse into the JSON `result` payload for the
// versioned envelope. Error maps to an INTERNAL ResponseError; the result-bearing
// Wire* variants serialize as-is, mirroring the bincode → JSON contract.
pub(crate) fn searchresponse_to_json_value(
    response: fff_ipc::types::SearchResponse,
) -> Result<serde_json::Value, fff_ipc::ResponseError> {
    use fff_ipc::types::SearchResponse;
    let internal = |e: serde_json::Error| fff_ipc::ResponseError {
        code: fff_ipc::INTERNAL.to_string(),
        message: format!("failed to serialize result: {e}"),
        engine_version: None,
        client_version: None,
    };
    match response {
        SearchResponse::GrepResults(w) => serde_json::to_value(w).map_err(internal),
        SearchResponse::SearchResults(v) | SearchResponse::RecentFiles(v) => {
            serde_json::to_value(v).map_err(internal)
        }
        SearchResponse::GitStatus(v) => serde_json::to_value(v).map_err(internal),
        SearchResponse::Directories(v) => serde_json::to_value(v).map_err(internal),
        SearchResponse::Health(h) => serde_json::to_value(h).map_err(internal),
        SearchResponse::Ack => Ok(serde_json::json!({ "ack": true })),
        SearchResponse::Error(message) => Err(fff_ipc::ResponseError {
            code: fff_ipc::INTERNAL.to_string(),
            message,
            engine_version: None,
            client_version: None,
        }),
    }
}
