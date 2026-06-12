"""Path parity (KTD8): Python's socket-path resolution must reproduce the Rust
``fff_ipc::paths`` precedence so the client finds the SAME ``master.sock`` the
engine created.

Documented cache-dir order (from ``crates/fff-ipc/src/paths.rs::xdg_cache_dir``):

    1. $XDG_CACHE_HOME (if set and non-empty)
    2. $HOME/.cache                         (XDG spec default)
    3. platform-canonical cache dir         (~/Library/Caches on macOS)
    4. /tmp

The master socket is then ``<cache>/fff/master.sock``.

This is pure Python: we compute the expected path from the documented order and
assert the client reproduces it across a monkeypatched env matrix. We cannot
call Rust, so the order itself is the contract under test.
"""

import sys
from pathlib import Path

import pytest

from fff_client import default_socket_paths
from fff_client._paths import master_socket_path, xdg_cache_dir

_SUFFIX = Path("fff") / "master.sock"


def _clear(monkeypatch, *names):
    for n in names:
        monkeypatch.delenv(n, raising=False)


def test_xdg_cache_home_wins(monkeypatch, tmp_path):
    xdg = tmp_path / "xdg-cache"
    monkeypatch.setenv("XDG_CACHE_HOME", str(xdg))
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    assert xdg_cache_dir() == xdg
    assert master_socket_path() == xdg / _SUFFIX


def test_empty_xdg_cache_home_falls_through_to_home(monkeypatch, tmp_path):
    # Rust treats an empty $XDG_CACHE_HOME as unset (the `!v.is_empty()` guard).
    monkeypatch.setenv("XDG_CACHE_HOME", "")
    home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(home))
    assert xdg_cache_dir() == home / ".cache"
    assert master_socket_path() == home / ".cache" / _SUFFIX


def test_home_cache_used_when_no_xdg(monkeypatch, tmp_path):
    _clear(monkeypatch, "XDG_CACHE_HOME")
    home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(home))
    assert xdg_cache_dir() == home / ".cache"


@pytest.mark.parametrize(
    "xdg, home, expected_kind",
    [
        ("/a/xdg", "/a/home", "xdg"),
        ("", "/b/home", "home"),
        (None, "/c/home", "home"),
    ],
)
def test_precedence_matrix(monkeypatch, xdg, home, expected_kind):
    if xdg is None:
        _clear(monkeypatch, "XDG_CACHE_HOME")
    else:
        monkeypatch.setenv("XDG_CACHE_HOME", xdg)
    monkeypatch.setenv("HOME", home)
    _clear(monkeypatch, "FFF_MASTER_SOCK")

    expected = Path(xdg) if expected_kind == "xdg" else Path(home) / ".cache"
    assert master_socket_path() == expected / _SUFFIX
    assert default_socket_paths() == (expected / _SUFFIX,)


def test_fff_master_sock_override_takes_precedence(monkeypatch, tmp_path):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "xdg"))
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    override = tmp_path / "custom" / "fff.sock"
    monkeypatch.setenv("FFF_MASTER_SOCK", str(override))

    paths = default_socket_paths()
    assert paths[0] == override  # override is highest precedence
    assert master_socket_path() in paths  # resolved default still offered as fallback


def test_constructor_master_sock_overrides_everything(monkeypatch, tmp_path):
    from fff_client import FffClient

    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "xdg"))
    monkeypatch.setenv("FFF_MASTER_SOCK", str(tmp_path / "env.sock"))
    explicit = tmp_path / "explicit.sock"

    client = FffClient(base_path="/repo", master_sock=str(explicit))
    # Only the constructor-provided socket is tried — env defaults are ignored.
    assert client._master_candidates() == [explicit]


@pytest.mark.skipif(
    sys.platform != "darwin", reason="macOS-specific platform-cache fallback"
)
def test_macos_home_cache_precedes_library_caches(monkeypatch, tmp_path):
    # On macOS the documented order reaches $HOME/.cache BEFORE ~/Library/Caches.
    _clear(monkeypatch, "XDG_CACHE_HOME")
    home = tmp_path / "home"
    monkeypatch.setenv("HOME", str(home))
    assert xdg_cache_dir() == home / ".cache"
    assert "Library/Caches" not in str(xdg_cache_dir())
