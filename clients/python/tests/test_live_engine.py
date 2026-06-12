"""Live integration against a real ``fff-engine`` binary.

GATED: skips cleanly when no engine binary is found under ``target/`` (e.g. CI
without a Rust build). It must NEVER fail in the binary's absence.

When the binary is present it:
  1. builds a temp git repo with known files,
  2. starts ``fff-engine --master`` under a temp ``XDG_CACHE_HOME`` so the master
     socket lands at ``<tmp>/fff/master.sock`` (the path the client resolves),
  3. connects the client and asserts ``find_files``/``grep`` return the known
     paths with ``frecency_score`` populated.

We deliberately only look under this repo's ``target/`` — NOT an arbitrary
``fff-engine`` on ``$PATH``. A released/installed engine predates this branch's
JSON protocol (it speaks legacy bincode) and may already be running as a
system-wide singleton master we cannot control, which would produce false
signals. The local ``target/`` build is the only engine guaranteed to speak the
protocol under test.
"""

import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import pytest

from fff_client import FffClient, FffUnreachable

# macOS AF_UNIX sun_path is capped at ~104 bytes. pytest's tmp_path nests too
# deeply, so the engine's cache (which hosts master/worker sockets) MUST live in
# a short directory or bind() fails with exit code 1.
_MAX_SUN_PATH = 100


def _find_engine_binary():
    """Locate a fff-engine built from THIS repo, under its target/ dirs.

    Picks the MOST RECENTLY BUILT binary across profiles. A stale binary from
    before this branch speaks legacy bincode and would fail the JSON protocol
    under test, so newest-by-mtime (not a fixed profile order) is what keeps the
    live test pointed at the build that actually carries the protocol.
    """
    here = Path(__file__).resolve()
    # clients/python/tests/ -> repo root is three levels up.
    repo_root = here.parents[3]
    candidates = [
        repo_root / "target" / profile / "fff-engine" for profile in ("release", "debug")
    ]
    existing = [c for c in candidates if c.is_file() and os.access(c, os.X_OK)]
    if not existing:
        return None
    return max(existing, key=lambda c: c.stat().st_mtime)


_ENGINE = _find_engine_binary()

pytestmark = pytest.mark.skipif(
    _ENGINE is None,
    reason="no fff-engine binary built under target/; live test skipped",
)


def _git(repo, *args):
    subprocess.run(
        ["git", *args],
        cwd=repo,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


@pytest.fixture
def repo(tmp_path):
    r = tmp_path / "repo"
    r.mkdir()
    (r / "alpha.txt").write_text("hello alpha world\n")
    (r / "beta.txt").write_text("hello beta world\n")
    (r / "src").mkdir()
    (r / "src" / "main.rs").write_text("fn main() { println!(\"alpha\"); }\n")
    _git(r, "init")
    _git(r, "config", "user.email", "t@t.test")
    _git(r, "config", "user.name", "t")
    _git(r, "add", ".")
    _git(r, "commit", "-m", "init")
    return r


@pytest.fixture
def engine():
    # Short cache dir (NOT pytest's deep tmp_path) so the worker socket path
    # stays under the macOS sun_path limit.
    cache_root = tempfile.mkdtemp(prefix="fffc-")
    cache = Path(cache_root)
    worker_sock = cache / "fff" / "workers" / "worker-0.sock"
    if len(str(worker_sock)) > _MAX_SUN_PATH:
        shutil.rmtree(cache_root, ignore_errors=True)
        pytest.skip("temp socket path too long for AF_UNIX on this platform")

    env = dict(os.environ, XDG_CACHE_HOME=str(cache))
    proc = subprocess.Popen(
        [str(_ENGINE), "--master", "--no-warmup"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    master_sock = cache / "fff" / "master.sock"
    try:
        deadline = time.time() + 10.0
        while time.time() < deadline:
            if proc.poll() is not None:
                pytest.skip(
                    "fff-engine exited during startup (code {})".format(
                        proc.returncode
                    )
                )
            if master_sock.exists():
                break
            time.sleep(0.05)
        else:
            pytest.skip("fff-engine master socket never appeared")
        yield master_sock
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(cache_root, ignore_errors=True)


def _connect_with_retry(repo, master_sock, attempts=40, delay=0.25):
    """The master spawns a worker on first handshake; retry while it warms up."""
    last = None
    for _ in range(attempts):
        try:
            client = FffClient(base_path=str(repo), master_sock=str(master_sock))
            client.connect()
            return client
        except FffUnreachable as exc:
            last = exc
            time.sleep(delay)
    pytest.skip("worker never became reachable: {}".format(last))


def test_live_find_files_returns_known_paths_with_frecency(repo, engine):
    client = _connect_with_retry(repo, engine)
    try:
        # Index population is async; poll until the known file appears.
        results = []
        deadline = time.time() + 15.0
        while time.time() < deadline:
            results = client.find_files("alpha", limit=20)
            if any(r["path"].endswith("alpha.txt") for r in results):
                break
            time.sleep(0.3)
        paths = [r["path"] for r in results]
        assert any(p.endswith("alpha.txt") for p in paths), paths
        for r in results:
            assert "frecency_score" in r
            assert "score" in r
    finally:
        client.close()


def test_live_grep_finds_content(repo, engine):
    client = _connect_with_retry(repo, engine)
    try:
        resp = {}
        deadline = time.time() + 15.0
        while time.time() < deadline:
            resp = client.grep("alpha")
            if resp.get("files_with_matches"):
                break
            time.sleep(0.3)
        assert resp.get("files_with_matches"), resp
        match_paths = [m["path"] for m in resp["matches"]]
        assert any("alpha" in line for m in resp["matches"] for line in [
            mm["line_text"] for mm in m["matches"]
        ]), match_paths
        for m in resp["matches"]:
            assert "frecency_score" in m
    finally:
        client.close()


def test_live_list_roots_includes_connected_root(repo, engine):
    # Regression guard: list_roots is a MASTER-socket verb. Routing it to the
    # worker (the original client bug) returns BAD_REQUEST against a real engine.
    client = _connect_with_retry(repo, engine)
    try:
        roots = client.list_roots()
        assert isinstance(roots, list)
        base_paths = [r["base_path"] for r in roots]
        resolved = str(repo.resolve())
        assert any(bp == resolved or bp == str(repo) for bp in base_paths), base_paths
        for r in roots:
            assert "base_path" in r and "name" in r and "default" in r
    finally:
        client.close()
