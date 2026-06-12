"""In-process fake engine speaking the documented wire protocol.

A stdlib ``socketserver.ThreadingUnixStreamServer`` binds temp Unix sockets and
answers framed JSON envelopes exactly as ``crates/fff-ipc`` documents:

    framing:  [4-byte LE u32 len][utf-8 json]
    request:  {"protocol_version", "verb", "params"}
    ok reply: {"protocol_version", "ok": true,  "result": ...}
    err reply:{"protocol_version", "ok": false, "error": {"code", "message"}}

Two-phase: master ``handshake {base_path}`` -> {worker_socket, worker_index};
worker ``connect {base_path}`` -> ack {"ok": true, "result": {"ack": true}}.
``record_access`` gets NO reply.

These tests are fully hermetic — no real fff-engine binary involved.
"""

import json
import os
import socket
import socketserver
import struct
import tempfile
import threading

import pytest

from fff_client import FffClient, FffError, FffUnreachable
from fff_client._protocol import PROTOCOL_VERSION

_LEN = struct.Struct("<I")


def _recv_exact(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof mid-frame")
        buf += chunk
    return buf


def _read_frame(sock):
    (length,) = _LEN.unpack(_recv_exact(sock, _LEN.size))
    return json.loads(_recv_exact(sock, length).decode("utf-8"))


def _write_frame(sock, payload):
    body = json.dumps(payload).encode("utf-8")
    sock.sendall(_LEN.pack(len(body)) + body)


# Canned engine replies, mirroring the Wire* JSON shapes.
_FIND_RESULT = [
    {"path": "src/main.rs", "score": 980, "git_status": "clean", "frecency_score": 42},
    {"path": "src/lib.rs", "score": 870, "git_status": "modified", "frecency_score": 17},
]
_GREP_RESULT = {
    "matches": [
        {
            "path": "src/main.rs",
            "size": 1234,
            "git_status": "clean",
            "frecency_score": 42,
            "matches": [
                {
                    "line_number": 1,
                    "col": 4,
                    "line_text": "fn main() {}",
                    "match_byte_offsets": [3, 7],
                    "is_definition": True,
                    "context_before": [],
                    "context_after": [],
                }
            ],
        }
    ],
    "total_files_searched": 2,
    "total_files": 2,
    "files_with_matches": 1,
    "next_file_offset": None,
    "regex_fallback_error": None,
}
_ROOTS_RESULT = [
    {"base_path": "/repo/a", "name": "a", "default": True},
    {"base_path": "/repo/b", "name": None, "default": False},
]


class _FakeEngine:
    """Master + worker fake on two temp Unix sockets.

    ``handshake_version`` / ``handshake_error`` drive the version-skew scenarios:
    when set, the master's handshake reply carries that version, or an error
    envelope, instead of the happy-path worker handout.
    """

    def __init__(self, *, handshake_version=PROTOCOL_VERSION, handshake_error=None):
        self._dir = tempfile.mkdtemp(prefix="fff-fake-")
        self.master_path = os.path.join(self._dir, "master.sock")
        self.worker_path = os.path.join(self._dir, "worker-0.sock")
        self._handshake_version = handshake_version
        self._handshake_error = handshake_error
        self.record_access_calls = []
        self._record_access_event = threading.Event()

        engine = self

        class MasterHandler(socketserver.BaseRequestHandler):
            def handle(self):
                req = _read_frame(self.request)
                verb = req.get("verb")
                # list_roots is a master-socket verb (one-shot), like handshake.
                if verb == "list_roots":
                    _write_frame(
                        self.request,
                        {
                            "protocol_version": PROTOCOL_VERSION,
                            "ok": True,
                            "result": _ROOTS_RESULT,
                        },
                    )
                    return
                if verb != "handshake":
                    return
                if engine._handshake_error is not None:
                    _write_frame(
                        self.request,
                        {
                            "protocol_version": PROTOCOL_VERSION,
                            "ok": False,
                            "error": engine._handshake_error,
                        },
                    )
                    return
                _write_frame(
                    self.request,
                    {
                        "protocol_version": engine._handshake_version,
                        "ok": True,
                        "result": {
                            "worker_socket": engine.worker_path,
                            "worker_index": 0,
                        },
                    },
                )

        class WorkerHandler(socketserver.BaseRequestHandler):
            def handle(self):
                while True:
                    try:
                        req = _read_frame(self.request)
                    except (ConnectionError, OSError, ValueError):
                        return
                    verb = req.get("verb")
                    if verb == "record_access":
                        engine.record_access_calls.append(req.get("params"))
                        engine._record_access_event.set()
                        continue  # NO reply, per protocol
                    _write_frame(self.request, engine._reply_for(verb))

        self._master = socketserver.ThreadingUnixStreamServer(
            self.master_path, MasterHandler
        )
        self._worker = socketserver.ThreadingUnixStreamServer(
            self.worker_path, WorkerHandler
        )
        self._master.daemon_threads = True
        self._worker.daemon_threads = True

    def _reply_for(self, verb):
        # list_roots is intentionally NOT here — it's a master-socket verb; a
        # real worker rejects it. Keeping it off the worker is what makes the
        # test exercise the correct (master) route.
        result = {
            "connect": {"ack": True},
            "find_files": _FIND_RESULT,
            "grep": _GREP_RESULT,
        }.get(verb, {})
        return {"protocol_version": PROTOCOL_VERSION, "ok": True, "result": result}

    def wait_record_access(self, timeout=2.0):
        return self._record_access_event.wait(timeout)

    def __enter__(self):
        threading.Thread(target=self._master.serve_forever, daemon=True).start()
        threading.Thread(target=self._worker.serve_forever, daemon=True).start()
        return self

    def __exit__(self, *exc):
        self._master.shutdown()
        self._worker.shutdown()
        self._master.server_close()
        self._worker.server_close()


def test_two_phase_handshake_and_find_files():
    with _FakeEngine() as engine:
        with FffClient(base_path="/repo/a", master_sock=engine.master_path) as fff:
            assert fff.worker_index == 0
            results = fff.find_files("main", limit=10)
            assert [r["path"] for r in results] == ["src/main.rs", "src/lib.rs"]
            assert results[0]["frecency_score"] == 42
            assert all("frecency_score" in r for r in results)


def test_find_files_sends_complete_options_object():
    """A partial caller override must be widened to a full options object.

    The engine's FindOptions has no per-field serde defaults, so a partial
    ``options`` like ``{"limit": 5}`` is rejected with BAD_REQUEST. The client
    must merge onto the full default set. We capture what the worker received.
    """
    received = {}

    with _FakeEngine() as engine:
        original = engine._reply_for

        def capturing(verb):
            return original(verb)

        engine._reply_for = capturing

        # Intercept the worker handler's parsed request by wrapping find reply.
        with FffClient(base_path="/repo/a", master_sock=engine.master_path) as fff:
            # Patch the client's _call to record the params actually sent.
            real_write = fff._call

            def spy(verb, params):
                if verb == "find_files":
                    received.update(params)
                return real_write(verb, params)

            fff._call = spy
            fff.find_files("main", limit=5)

    opts = received["options"]
    assert opts["limit"] == 5  # caller override preserved
    # Every engine-required field is present (not just the override).
    for field in ("max_threads", "offset", "combo_boost_score_multiplier", "min_combo_count"):
        assert field in opts, field


def test_grep_resolves_typed_response():
    with _FakeEngine() as engine:
        with FffClient(base_path="/repo/a", master_sock=engine.master_path) as fff:
            resp = fff.grep("main")
            assert resp["files_with_matches"] == 1
            match = resp["matches"][0]
            assert match["path"] == "src/main.rs"
            assert match["frecency_score"] == 42
            assert match["matches"][0]["is_definition"] is True


def test_record_access_sends_without_blocking_and_connection_stays_usable():
    with _FakeEngine() as engine:
        with FffClient(
            base_path="/repo/a",
            master_sock=engine.master_path,
            record_access_enabled=True,
        ) as fff:
            # Returns immediately; the engine sends no reply for this verb.
            fff.record_access("src/main.rs")
            assert engine.wait_record_access(), "engine never observed record_access"
            assert engine.record_access_calls[-1] == {"path": "src/main.rs"}

            # The worker connection is still usable for a normal request/reply.
            results = fff.find_files("main")
            assert results[0]["path"] == "src/main.rs"


def test_record_access_disabled_is_noop():
    with _FakeEngine() as engine:
        with FffClient(base_path="/repo/a", master_sock=engine.master_path) as fff:
            fff.record_access("src/main.rs")  # default record_access_enabled=False
            assert not engine.wait_record_access(timeout=0.3)
            assert engine.record_access_calls == []


def test_list_roots_returns_canned_roots():
    with _FakeEngine() as engine:
        with FffClient(base_path="/repo/a", master_sock=engine.master_path) as fff:
            roots = fff.list_roots()
            assert [r["base_path"] for r in roots] == ["/repo/a", "/repo/b"]
            assert roots[0]["default"] is True
            assert roots[1]["name"] is None


# ── version skew (R3) ──────────────────────────────────────────────────────


def test_version_skew_handshake_version_raises_protocol_mismatch():
    with _FakeEngine(handshake_version=PROTOCOL_VERSION + 1) as engine:
        with pytest.raises(FffError) as exc:
            FffClient(base_path="/repo/a", master_sock=engine.master_path).connect()
        assert exc.value.code == "PROTOCOL_MISMATCH"


def test_version_skew_error_envelope_raises_protocol_mismatch_no_partial():
    err = {
        "code": "PROTOCOL_MISMATCH",
        "message": "engine speaks 2, client sent 1",
        "engine_version": 2,
        "client_version": 1,
    }
    with _FakeEngine(handshake_error=err) as engine:
        client = FffClient(base_path="/repo/a", master_sock=engine.master_path)
        with pytest.raises(FffError) as exc:
            client.connect()
        assert exc.value.code == "PROTOCOL_MISMATCH"
        # No partial connection: the worker was never bound.
        assert client.worker_index is None
        with pytest.raises(FffError):
            client.find_files("main")  # not connected -> errors, no partial result


# ── unreachable (KTD6) ──────────────────────────────────────────────────────


def test_nonexistent_socket_raises_unreachable():
    missing = os.path.join(tempfile.mkdtemp(prefix="fff-missing-"), "master.sock")
    assert not os.path.exists(missing)
    with pytest.raises(FffUnreachable):
        FffClient(base_path="/repo/a", master_sock=missing).connect()


def test_refused_socket_raises_unreachable():
    # A bound-but-not-listening AF_UNIX path: connect() refuses.
    d = tempfile.mkdtemp(prefix="fff-refuse-")
    path = os.path.join(d, "master.sock")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.bind(path)  # bound, never listen()/accept()
    try:
        with pytest.raises(FffUnreachable):
            FffClient(
                base_path="/repo/a", master_sock=path, timeout=1.0
            ).connect()
    finally:
        s.close()
