"""``FffClient``: two-phase handshake, verbs, context manager (connect-and-fail)."""

import socket
from pathlib import Path
from typing import Any, Dict, List, Optional, Union

from . import _protocol
from ._paths import default_socket_paths

__all__ = ["FffError", "FffUnreachable", "FffClient"]


class FffError(Exception):
    """A typed error returned by the engine or raised by the client.

    ``code`` mirrors the wire ``error.code`` (e.g. ``PROTOCOL_MISMATCH``,
    ``BAD_REQUEST``, ``INTERNAL``); ``message`` is the human-readable text.
    """

    def __init__(self, code: str, message: str):
        super().__init__("{}: {}".format(code, message))
        self.code = code
        self.message = message


class FffUnreachable(FffError):
    """The daemon socket is absent or refuses the connection (KTD6, no spawn)."""

    def __init__(self, message: str):
        super().__init__("UNREACHABLE", message)


# Default per-operation socket timeout (seconds). Bounds connect-and-fail so an
# absent or wedged daemon surfaces promptly rather than hanging.
_DEFAULT_TIMEOUT = 10.0


class FffClient:
    """Stdlib-only client for the fff engine daemon's versioned JSON protocol.

    Connect-and-fail (KTD6): never spawns a daemon. If the master socket is
    absent or refuses, :class:`FffUnreachable` is raised. On a version-skew
    handshake reply, :class:`FffError` with ``code="PROTOCOL_MISMATCH"`` is
    raised.

    Use as a context manager::

        with FffClient(base_path="/path/to/repo") as fff:
            for hit in fff.find_files("main"):
                print(hit["path"], hit["score"])

    or manage the lifecycle explicitly via :meth:`connect` / :meth:`close`.
    """

    def __init__(
        self,
        base_path: Union[str, Path],
        *,
        master_sock: Optional[Union[str, Path]] = None,
        record_access_enabled: bool = False,
        timeout: float = _DEFAULT_TIMEOUT,
    ):
        self.base_path = str(base_path)
        self._master_sock = Path(master_sock) if master_sock is not None else None
        self.record_access_enabled = record_access_enabled
        self._timeout = timeout
        self._worker: Optional[socket.socket] = None
        self._worker_index: Optional[int] = None

    # ── lifecycle ──────────────────────────────────────────────────────────

    def __enter__(self) -> "FffClient":
        self.connect()
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def connect(self) -> "FffClient":
        """Run the two-phase handshake and bind to ``base_path``'s worker."""
        if self._worker is not None:
            return self

        master = self._connect_socket(self._master_candidates())
        try:
            _protocol.write_message(
                master, _protocol.build_request("handshake", {"base_path": self.base_path})
            )
            result = self._read_result(master)
        finally:
            master.close()

        worker_path = result["worker_socket"]
        self._worker_index = result.get("worker_index")

        worker = self._connect_socket([Path(worker_path)])
        try:
            _protocol.write_message(
                worker, _protocol.build_request("connect", {"base_path": self.base_path})
            )
            self._read_result(worker)
        except BaseException:
            worker.close()
            raise
        self._worker = worker
        return self

    def close(self) -> None:
        """Close the worker connection. Idempotent."""
        if self._worker is not None:
            try:
                self._worker.close()
            finally:
                self._worker = None
                self._worker_index = None

    @property
    def worker_index(self) -> Optional[int]:
        return self._worker_index

    # ── verbs ──────────────────────────────────────────────────────────────

    def grep(self, query: str, **options: Any) -> Dict[str, Any]:
        """Content search. Returns a ``WireGrepResponse`` dict with keys:
        ``matches`` (list of ``{path, size, git_status, frecency_score,
        matches: [{line_number, col, line_text, match_byte_offsets,
        is_definition, context_before, context_after}]}``),
        ``total_files_searched``, ``total_files``, ``files_with_matches``,
        ``next_file_offset``, ``regex_fallback_error``.

        ``options`` keys mirror the Rust ``GrepOptions`` (e.g. ``smart_case``,
        ``mode`` in ``{"PlainText","Regex","Fuzzy"}``, ``page_limit``,
        ``max_matches_per_file``, ``before_context``, ``after_context``).
        Unset fields take the engine-side defaults.
        """
        params: Dict[str, Any] = {"query": query}
        if options:
            params["options"] = _protocol.merged_options(
                _protocol.GREP_OPTIONS_DEFAULTS, options
            )
        return self._call("grep", params)

    def find_files(self, query: str, **options: Any) -> List[Dict[str, Any]]:
        """Fuzzy file search. Returns a list of ``WireSearchResult`` dicts:
        ``{path, score, git_status, frecency_score}``, ranked.

        ``options`` mirror the Rust ``FindOptions`` (e.g. ``limit``,
        ``offset``, ``current_file``). A bare ``limit=N`` is the common case.
        """
        params: Dict[str, Any] = {"query": query}
        if options:
            params["options"] = _protocol.merged_options(
                _protocol.FIND_OPTIONS_DEFAULTS, options
            )
        return self._call("find_files", params)

    def multi_grep(
        self,
        patterns: List[str],
        constraints: Optional[str] = None,
        **options: Any,
    ) -> Dict[str, Any]:
        """OR-search across ``patterns``. ``constraints`` is a raw constraint
        string (e.g. ``"*.rs !test/"``). Returns a ``WireGrepResponse`` dict
        (same shape as :meth:`grep`).
        """
        params: Dict[str, Any] = {"patterns": list(patterns)}
        if constraints is not None:
            params["constraints"] = constraints
        if options:
            params["options"] = _protocol.merged_options(
                _protocol.GREP_OPTIONS_DEFAULTS, options
            )
        return self._call("multi_grep", params)

    def list_roots(self) -> List[Dict[str, Any]]:
        """Enumerate targetable roots. Returns a list of ``WireRoot`` dicts:
        ``{base_path, name (str|None), default (bool)}``, default-first.
        """
        return self._call("list_roots", {})

    def list_recent_files(
        self, limit: int, dirty_only: bool = False
    ) -> List[Dict[str, Any]]:
        """Top-``limit`` files by frecency. Returns ``WireSearchResult`` dicts.
        ``dirty_only`` restricts to files with a non-clean git status.
        """
        return self._call(
            "list_recent_files", {"limit": limit, "dirty_only": dirty_only}
        )

    def get_git_status(self, include_clean: bool = False) -> List[Dict[str, Any]]:
        """Files with a notable git status. Returns ``WireGitFile`` dicts:
        ``{path, status, frecency_score}``. ``include_clean`` adds clean files.
        """
        return self._call("get_git_status", {"include_clean": include_clean})

    def list_directories(self, limit: int) -> List[Dict[str, Any]]:
        """Top-``limit`` directories by peak child frecency. Returns
        ``WireDirEntry`` dicts: ``{path, max_frecency}``.
        """
        return self._call("list_directories", {"limit": limit})

    def health(self) -> Dict[str, Any]:
        """Index-freshness snapshot. Returns a ``HealthResponse`` dict:
        ``{roots: [{slug, base_path, indexed_files, last_scan_age_sec,
        watcher_backlog, dirty_count}]}``.
        """
        return self._call("health", {})

    def record_access(self, path: str) -> None:
        """Fire-and-forget frecency write — opt-in, default OFF (KTD7).

        A no-op unless ``record_access_enabled=True`` was set on the client.
        When enabled, the frame is sent and NO reply is read (the engine sends
        none for this verb).
        """
        if not self.record_access_enabled:
            return
        sock = self._require_worker()
        _protocol.write_message(
            sock, _protocol.build_request("record_access", {"path": path})
        )

    # ── internals ────────────────────────────────────────────────────────────

    def _master_candidates(self) -> List[Path]:
        if self._master_sock is not None:
            return [self._master_sock]
        return list(default_socket_paths())

    def _connect_socket(self, candidates: List[Path]) -> socket.socket:
        last_error: Optional[OSError] = None
        for path in candidates:
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock.settimeout(self._timeout)
            try:
                sock.connect(str(path))
                return sock
            except OSError as exc:
                last_error = exc
                sock.close()
        tried = ", ".join(str(p) for p in candidates)
        raise FffUnreachable(
            "no fff daemon reachable (tried: {}): {}".format(tried, last_error)
        )

    def _read_result(self, sock: socket.socket) -> Any:
        envelope = _protocol.read_message(sock)
        self._check_version(envelope)
        if envelope.get("ok"):
            return envelope.get("result")
        error = envelope.get("error") or {}
        raise FffError(
            error.get("code", _protocol.INTERNAL),
            error.get("message", "engine returned an error"),
        )

    def _check_version(self, envelope: Dict[str, Any]) -> None:
        version = envelope.get("protocol_version")
        if version is not None and version != _protocol.PROTOCOL_VERSION:
            raise FffError(
                _protocol.PROTOCOL_MISMATCH,
                "protocol version mismatch: client speaks {}, engine sent {}".format(
                    _protocol.PROTOCOL_VERSION, version
                ),
            )

    def _require_worker(self) -> socket.socket:
        if self._worker is None:
            raise FffError(
                _protocol.INTERNAL, "client is not connected; call connect() first"
            )
        return self._worker

    def _call(self, verb: str, params: Dict[str, Any]) -> Any:
        sock = self._require_worker()
        _protocol.write_message(sock, _protocol.build_request(verb, params))
        return self._read_result(sock)
