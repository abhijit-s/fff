"""Wire protocol: framing, envelope build/parse, version check.

Mirrors ``crates/fff-ipc/src/protocol.rs`` and ``codec.rs``.

Framing:  ``[4-byte little-endian u32 payload length][payload bytes]``.
Payload:  UTF-8 JSON.

Request envelope:
    ``{"protocol_version": <int>, "verb": "<name>", "params": {…}}``
Response (ok):
    ``{"protocol_version": <int>, "ok": true, "result": <…>}``
Response (err):
    ``{"protocol_version": <int>, "ok": false,
       "error": {"code", "message", "engine_version"?, "client_version"?}}``
"""

import json
import socket
import struct
from typing import Any, Dict, Optional

__all__ = [
    "PROTOCOL_VERSION",
    "MAX_FRAME_LEN",
    "PROTOCOL_MISMATCH",
    "BAD_REQUEST",
    "INTERNAL",
    "FIND_OPTIONS_DEFAULTS",
    "GREP_OPTIONS_DEFAULTS",
    "merged_options",
    "build_request",
    "encode_frame",
    "write_message",
    "read_message",
]

# Single source of truth for the wire version — mirrors fff_ipc::PROTOCOL_VERSION.
PROTOCOL_VERSION = 1

# Upper bound on a declared frame length (64 MiB), guarding against a hostile or
# garbled length prefix (KTD9). Mirrors fff_ipc::protocol::MAX_FRAME_LEN.
MAX_FRAME_LEN = 64 * 1024 * 1024

# Error codes (mirror protocol.rs).
PROTOCOL_MISMATCH = "PROTOCOL_MISMATCH"
BAD_REQUEST = "BAD_REQUEST"
INTERNAL = "INTERNAL"

_LEN_PREFIX = struct.Struct("<I")

# The engine's `FindOptions`/`GrepOptions` have NO per-field serde defaults: when
# the `options` key is present it must carry EVERY field, or deserialization
# fails ("missing field ..."). Omitting `options` entirely is fine (the field is
# `#[serde(default)]`), but a partial object is rejected. These mirror the Rust
# `Default` impls in fff_ipc::types so the client can send a complete object when
# the caller overrides any single field.
FIND_OPTIONS_DEFAULTS = {
    "max_threads": 0,
    "current_file": None,
    "combo_boost_score_multiplier": 3,
    "min_combo_count": 2,
    "offset": 0,
    "limit": 20,
}

GREP_OPTIONS_DEFAULTS = {
    "max_file_size": 10 * 1024 * 1024,
    "max_matches_per_file": 10,
    "smart_case": True,
    "file_offset": 0,
    "page_limit": 50,
    "mode": "PlainText",
    "time_budget_ms": 0,
    "before_context": 0,
    "after_context": 0,
    "classify_definitions": False,
    "trim_whitespace": True,
}


def merged_options(
    defaults: Dict[str, Any], overrides: Dict[str, Any]
) -> Dict[str, Any]:
    """Full option object: engine defaults with caller overrides applied."""
    merged = dict(defaults)
    merged.update(overrides)
    return merged


def build_request(verb: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """Build a request envelope with the pinned protocol version."""
    return {
        "protocol_version": PROTOCOL_VERSION,
        "verb": verb,
        "params": params if params is not None else {},
    }


def encode_frame(payload: Dict[str, Any]) -> bytes:
    """Serialize a JSON envelope to ``[u32-LE len][utf-8 json]`` bytes."""
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    if len(body) > MAX_FRAME_LEN:
        raise ValueError(
            "outgoing frame {} bytes exceeds MAX_FRAME_LEN {}".format(
                len(body), MAX_FRAME_LEN
            )
        )
    return _LEN_PREFIX.pack(len(body)) + body


def write_message(sock: socket.socket, payload: Dict[str, Any]) -> None:
    """Frame and send a JSON envelope over ``sock``."""
    sock.sendall(encode_frame(payload))


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    """Read exactly ``n`` bytes or raise ``ConnectionError`` on early EOF."""
    chunks = []
    remaining = n
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ConnectionError(
                "connection closed mid-frame: wanted {} more byte(s)".format(remaining)
            )
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def read_message(sock: socket.socket) -> Dict[str, Any]:
    """Read one framed JSON envelope, enforcing the max-frame guard (KTD9)."""
    header = _recv_exact(sock, _LEN_PREFIX.size)
    (length,) = _LEN_PREFIX.unpack(header)
    if length > MAX_FRAME_LEN:
        raise ValueError(
            "declared frame length {} exceeds MAX_FRAME_LEN {}".format(
                length, MAX_FRAME_LEN
            )
        )
    body = _recv_exact(sock, length)
    return json.loads(body.decode("utf-8"))
