"""Socket-path resolution mirroring ``fff_ipc::paths`` (KTD8).

The master socket lives at ``<cache>/fff/master.sock``. The ``<cache>`` lookup
order mirrors the Rust ``xdg_cache_dir`` precedence EXACTLY so the Python client
resolves the SAME socket the engine created:

    1. ``$XDG_CACHE_HOME`` (if set and non-empty)
    2. ``$HOME/.cache``                          (XDG spec default)
    3. platform-canonical cache dir             (e.g. ~/Library/Caches on macOS)
    4. ``/tmp``

On macOS the Rust code reaches ``$HOME/.cache`` *before* the platform dir, so
the order above is reproduced verbatim rather than the macOS-canonical order.
"""

import os
import sys
from pathlib import Path
from typing import Tuple

__all__ = ["xdg_cache_dir", "master_socket_path", "default_socket_paths"]


def _home_dir() -> "Path | None":
    home = os.environ.get("HOME")
    if home:
        return Path(home)
    try:
        return Path.home()
    except (RuntimeError, KeyError):
        return None


def _platform_cache_dir() -> "Path | None":
    """Platform-canonical cache dir, mirroring the Rust ``dirs::cache_dir()``."""
    if sys.platform == "darwin":
        home = _home_dir()
        return home / "Library" / "Caches" if home else None
    if os.name == "nt":
        local = os.environ.get("LOCALAPPDATA")
        return Path(local) if local else None
    # Other unixes: dirs::cache_dir() == $XDG_CACHE_HOME or $HOME/.cache,
    # already covered by the earlier branches; nothing extra to add.
    return None


def xdg_cache_dir() -> Path:
    """Resolve the cache directory using the Rust ``xdg_cache_dir`` precedence."""
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return Path(xdg)

    home = _home_dir()
    if home is not None:
        return home / ".cache"

    platform = _platform_cache_dir()
    if platform is not None:
        return platform

    return Path("/tmp")


def master_socket_path() -> Path:
    """The master Unix socket: ``<cache>/fff/master.sock``."""
    return xdg_cache_dir() / "fff" / "master.sock"


def default_socket_paths() -> Tuple[Path, ...]:
    """Candidate master-socket paths, highest precedence first.

    Honors ``$FFF_MASTER_SOCK`` as an explicit override that takes priority over
    the resolved default. Returns a tuple so callers can try each in order.
    """
    candidates = []
    override = os.environ.get("FFF_MASTER_SOCK")
    if override:
        candidates.append(Path(override))
    candidates.append(master_socket_path())

    seen = set()
    unique = []
    for path in candidates:
        if path not in seen:
            seen.add(path)
            unique.append(path)
    return tuple(unique)
