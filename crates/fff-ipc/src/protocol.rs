//! Versioned, language-neutral JSON wire envelope (reference implementation).
//!
//! This is the public contract spoken by the engine's JSON dual-read path, the
//! migrated `fff-mcp` client, and the stdlib-only Python client. It rides over
//! the SAME `[u32-LE len][payload]` framing as the bincode path (see `codec`),
//! distinguished only by a first-byte content sniff (`looks_like_json`).
//!
//! Envelope shapes (directional):
//!
//! ```text
//! request:      { "protocol_version": <u32>, "verb": "<name>", "params": { … } }
//! response ok:  { "protocol_version": <u32>, "ok": true,  "result": <verb-specific JSON> }
//! response err: { "protocol_version": <u32>, "ok": false, "error": {
//!                   "code": "<CODE>", "message": "<text>",
//!                   "engine_version"?: <u32>, "client_version"?: <u32> } }
//! ```
//!
//! `verb` is a string tag (NOT a bincode ordinal), so the wire is
//! reorder-resilient. `params` and `result` are free-form JSON objects: handlers
//! deserialize `params` into the existing `GrepOptions`/`FindOptions`/`Wire*`
//! types, and serialize results back as those same serde-derived structs. No
//! parallel result types exist.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Single source of truth for the wire protocol version. Bumped on any
/// breaking change to the envelope or verb/result shapes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound on a declared JSON frame length (64 MiB). The JSON read path
/// rejects an over-length declared frame with a typed error rather than
/// allocating, guarding against a hostile/garbled length prefix (KTD9).
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

// ── Error codes ───────────────────────────────────────────────────────────────

/// Returned when an incoming `protocol_version` is incompatible with this
/// engine's `PROTOCOL_VERSION`. The error carries both versions.
pub const PROTOCOL_MISMATCH: &str = "PROTOCOL_MISMATCH";
/// The envelope or its params could not be understood (malformed verb, missing
/// fields, undeserializable params).
pub const BAD_REQUEST: &str = "BAD_REQUEST";
/// The handler failed while processing an otherwise valid request.
pub const INTERNAL: &str = "INTERNAL";

// ── Verb tags ───────────────────────────────────────────────────────────────

/// Verb string constants — the on-wire `verb` field values.
pub mod verbs {
    pub const HANDSHAKE: &str = "handshake";
    pub const CONNECT: &str = "connect";
    pub const GREP: &str = "grep";
    pub const FIND_FILES: &str = "find_files";
    pub const MULTI_GREP: &str = "multi_grep";
    pub const LIST_RECENT_FILES: &str = "list_recent_files";
    pub const GET_GIT_STATUS: &str = "get_git_status";
    pub const LIST_DIRECTORIES: &str = "list_directories";
    pub const RECORD_ACCESS: &str = "record_access";
    pub const LIST_ROOTS: &str = "list_roots";
    pub const HEALTH: &str = "health";
}

// ── Request envelope ────────────────────────────────────────────────────────

/// The on-wire request envelope. `params` is a free-form JSON object whose shape
/// is determined by `verb`; handlers deserialize it into the matching params
/// type. See the per-verb `*Params` structs below for the documented shapes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub verb: String,
    #[serde(default)]
    pub params: Value,
}

impl RequestEnvelope {
    pub fn new(verb: &str, params: Value) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            verb: verb.to_string(),
            params,
        }
    }

    /// Deserialize `params` into a concrete verb-params type. Maps any serde
    /// failure to a `BAD_REQUEST` error envelope so callers can reply uniformly.
    pub fn params_as<T: for<'de> Deserialize<'de>>(&self) -> Result<T, ResponseEnvelope> {
        serde_json::from_value(self.params.clone()).map_err(|e| {
            ResponseEnvelope::err(ResponseError {
                code: BAD_REQUEST.to_string(),
                message: format!("invalid params for verb '{}': {e}", self.verb),
                engine_version: None,
                client_version: None,
            })
        })
    }
}

// Per-verb param shapes. Reuse `GrepOptions`/`FindOptions` from types.rs verbatim.

/// Params for `handshake` and `connect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BasePathParams {
    pub base_path: String,
}

/// Params for `grep` and `find_files` use `query` + the matching options type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrepParams {
    pub query: String,
    #[serde(default)]
    pub options: crate::types::GrepOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindFilesParams {
    pub query: String,
    #[serde(default)]
    pub options: crate::types::FindOptions,
}

/// Params for `multi_grep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiGrepParams {
    pub patterns: Vec<String>,
    #[serde(default)]
    pub constraints: Option<String>,
    #[serde(default)]
    pub options: crate::types::GrepOptions,
}

/// Params for `list_recent_files`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRecentFilesParams {
    pub limit: usize,
    #[serde(default)]
    pub dirty_only: bool,
}

/// Params for `get_git_status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetGitStatusParams {
    #[serde(default)]
    pub include_clean: bool,
}

/// Params for `list_directories`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDirectoriesParams {
    pub limit: usize,
}

/// Params for `record_access`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordAccessParams {
    pub path: String,
}

// `list_roots` and `health` take empty params (`{}`).

// ── Result payloads ───────────────────────────────────────────────────────

/// Result for the `handshake` verb — the worker socket to use next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResult {
    pub worker_socket: String,
    pub worker_index: u32,
}

/// One root in the `list_roots` result. Configured `[mcp]` roots carry a `name`
/// and `default`; a live-but-unconfigured base_path has `name: None`,
/// `default: false`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRoot {
    pub base_path: String,
    #[serde(default)]
    pub name: Option<String>,
    pub default: bool,
}

// ── Response envelope ───────────────────────────────────────────────────────

/// The on-wire response envelope. Exactly one of `result` (when `ok`) or `error`
/// (when `!ok`) is present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<u32>,
}

impl ResponseEnvelope {
    /// Build an ok response carrying a serializable verb-specific result.
    pub fn ok<T: Serialize>(result: &T) -> Result<Self, serde_json::Error> {
        Ok(Self {
            protocol_version: PROTOCOL_VERSION,
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    /// Build an error response from a `ResponseError`.
    pub fn err(error: ResponseError) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

// ── Version check + sniff helpers ─────────────────────────────────────────────

/// Refuse-on-mismatch version check (KTD3/KTD4). On skew, returns a
/// `PROTOCOL_MISMATCH` error envelope carrying both versions; the caller writes
/// it and closes the connection. `Ok(())` means compatible.
pub fn check_protocol_version(client_version: u32) -> Result<(), ResponseEnvelope> {
    if client_version == PROTOCOL_VERSION {
        return Ok(());
    }
    Err(ResponseEnvelope::err(ResponseError {
        code: PROTOCOL_MISMATCH.to_string(),
        message: format!(
            "protocol version mismatch: engine speaks {PROTOCOL_VERSION}, client sent {client_version}"
        ),
        engine_version: Some(PROTOCOL_VERSION),
        client_version: Some(client_version),
    }))
}

/// Dual-read content sniff (KTD1): true iff the first non-whitespace byte is
/// `{`. Legacy bincode first-frames begin with a small variant ordinal byte
/// (reachable values 0x00–0x0A), never 0x7B, so the test is collision-free.
pub fn looks_like_json(payload: &[u8]) -> bool {
    payload.iter().find(|b| !b.is_ascii_whitespace()).copied() == Some(b'{')
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IpcError;
    use crate::codec::{read_json_message_sync, write_json_message_sync};
    use crate::types::{GrepOptions, WireGrepResponse};
    use std::io::Cursor;

    #[test]
    fn grep_request_envelope_round_trips() {
        let req = RequestEnvelope::new(
            verbs::GREP,
            serde_json::to_value(GrepParams {
                query: "héllo".into(),
                options: GrepOptions::default(),
            })
            .unwrap(),
        );

        let mut buf = Vec::new();
        write_json_message_sync(&mut buf, &req).unwrap();

        let mut cursor = Cursor::new(buf);
        let rt: RequestEnvelope = read_json_message_sync(&mut cursor).unwrap();
        assert_eq!(rt.protocol_version, PROTOCOL_VERSION);
        assert_eq!(rt.verb, verbs::GREP);
        let params: GrepParams = rt.params_as().unwrap();
        assert_eq!(params.query, "héllo");
    }

    #[test]
    fn ok_response_round_trips() {
        let result = WireGrepResponse {
            matches: vec![],
            total_files_searched: 3,
            total_files: 10,
            files_with_matches: 0,
            next_file_offset: 0,
            regex_fallback_error: None,
        };
        let resp = ResponseEnvelope::ok(&result).unwrap();
        assert!(resp.ok);

        let mut buf = Vec::new();
        write_json_message_sync(&mut buf, &resp).unwrap();
        let mut cursor = Cursor::new(buf);
        let rt: ResponseEnvelope = read_json_message_sync(&mut cursor).unwrap();
        assert!(rt.ok);
        assert!(rt.error.is_none());
        let parsed: WireGrepResponse = serde_json::from_value(rt.result.unwrap()).unwrap();
        assert_eq!(parsed.total_files, 10);
    }

    #[test]
    fn err_response_round_trips() {
        let resp = ResponseEnvelope::err(ResponseError {
            code: INTERNAL.to_string(),
            message: "boom".into(),
            engine_version: None,
            client_version: None,
        });
        assert!(!resp.ok);

        let mut buf = Vec::new();
        write_json_message_sync(&mut buf, &resp).unwrap();
        let mut cursor = Cursor::new(buf);
        let rt: ResponseEnvelope = read_json_message_sync(&mut cursor).unwrap();
        assert!(!rt.ok);
        assert!(rt.result.is_none());
        let err = rt.error.unwrap();
        assert_eq!(err.code, INTERNAL);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn looks_like_json_table() {
        // JSON first byte (with and without leading whitespace).
        assert!(looks_like_json(b"{\"verb\":\"grep\"}"));
        assert!(looks_like_json(b"  \n\t{}"));
        // Every reachable legacy bincode first ordinal byte 0x00..=0x0A.
        for ordinal in 0u8..=0x0A {
            let frame = [ordinal, 0, 0, 0];
            assert!(
                !looks_like_json(&frame),
                "ordinal {ordinal:#x} must not sniff as JSON"
            );
        }
        // Empty payload is not JSON.
        assert!(!looks_like_json(b""));
    }

    #[test]
    fn over_length_frame_is_rejected_without_allocating() {
        // Declared length one past MAX_FRAME_LEN; no payload bytes follow.
        let declared = MAX_FRAME_LEN + 1;
        let frame = declared.to_le_bytes();
        let mut cursor = Cursor::new(frame.to_vec());
        let result: Result<RequestEnvelope, _> = read_json_message_sync(&mut cursor);
        match result {
            Err(IpcError::FrameTooLarge { len, max }) => {
                assert_eq!(len, declared);
                assert_eq!(max, MAX_FRAME_LEN);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn version_mismatch_helper_produces_protocol_mismatch_envelope() {
        let err_env = check_protocol_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(!err_env.ok);
        let err = err_env.error.unwrap();
        assert_eq!(err.code, PROTOCOL_MISMATCH);
        assert_eq!(err.engine_version, Some(PROTOCOL_VERSION));
        assert_eq!(err.client_version, Some(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn matching_version_passes() {
        assert!(check_protocol_version(PROTOCOL_VERSION).is_ok());
    }
}
