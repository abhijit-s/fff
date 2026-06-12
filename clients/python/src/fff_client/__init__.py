"""Stdlib-only Python client for fff's versioned engine daemon protocol.

Connects to the warm, shared, frecency-ranked engine surface the MCP tools use,
over a versioned JSON wire protocol. Connect-and-fail: never spawns a daemon.

    from fff_client import FffClient

    with FffClient(base_path="/path/to/repo") as fff:
        results = fff.find_files("main", limit=10)
        for r in results:
            print(r["path"], r["score"], r["frecency_score"])
"""

from ._client import FffClient, FffError, FffUnreachable
from ._paths import default_socket_paths
from ._protocol import PROTOCOL_VERSION

__all__ = [
    "FffClient",
    "FffError",
    "FffUnreachable",
    "default_socket_paths",
    "PROTOCOL_VERSION",
]

__version__ = "0.1.0"
