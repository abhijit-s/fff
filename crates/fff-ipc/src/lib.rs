pub mod codec;
pub mod config;
pub mod lockfile;
pub mod paths;
pub mod protocol;
pub mod routing;
pub mod types;

pub use codec::{
    decode_bincode, decode_json, read_frame, read_frame_sync, read_json_message,
    read_json_message_sync, read_message, read_message_sync, write_json_message,
    write_json_message_sync, write_message, write_message_sync,
};
#[cfg(unix)]
pub use paths::wait_for_socket;
pub use paths::{
    base_path_slug, lockfile_path, log_path, master_lockfile_path, master_socket_path,
    routing_table_path, socket_path, worker_lockfile_path, worker_socket_path, xdg_cache_dir,
    xdg_data_dir, xdg_runtime_dir,
};
pub use protocol::{
    BAD_REQUEST, BasePathParams, FindFilesParams, GetGitStatusParams, GrepParams, HandshakeResult,
    INTERNAL, ListDirectoriesParams, ListRecentFilesParams, MAX_FRAME_LEN, MultiGrepParams,
    PROTOCOL_MISMATCH, PROTOCOL_VERSION, RecordAccessParams, RequestEnvelope, ResponseEnvelope,
    ResponseError, WireRoot, check_protocol_version, looks_like_json,
};
pub use routing::{RoutingTable, SerializableRing, WorkerEntry};
pub use types::*;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode error: {0}")]
    Encode(#[source] Box<bincode::ErrorKind>),
    #[error("decode error: {0}")]
    Decode(#[source] Box<bincode::ErrorKind>),
    #[error("JSON encode error: {0}")]
    JsonEncode(#[source] serde_json::Error),
    #[error("JSON decode error: {0}")]
    JsonDecode(#[source] serde_json::Error),
    #[error("frame too large: declared {len} bytes exceeds max {max}")]
    FrameTooLarge { len: u32, max: u32 },
    #[error("protocol error: {0}")]
    Protocol(String),
}
