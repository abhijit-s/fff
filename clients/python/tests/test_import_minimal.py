"""Smoke test: the package imports stdlib-only and exports its public surface.

The full round-trip / fake-engine / version-skew / path-parity matrix lives in
the sibling test modules. This file pins the structural guarantee that importing
``fff_client`` pulls in NO third-party package (the basis for the 3.9-compat
claim: stdlib-only imports are valid back to 3.9 regardless of the interpreter
running this suite).
"""

import sys


def test_public_surface():
    import fff_client

    assert fff_client.PROTOCOL_VERSION == 1
    for name in ("FffClient", "FffError", "FffUnreachable", "default_socket_paths"):
        assert hasattr(fff_client, name), name


def test_unreachable_is_ffferror_subclass():
    from fff_client import FffError, FffUnreachable

    assert issubclass(FffUnreachable, FffError)


def test_default_socket_paths_returns_tuple_of_paths():
    from pathlib import Path

    from fff_client import default_socket_paths

    paths = default_socket_paths()
    assert isinstance(paths, tuple)
    assert all(isinstance(p, Path) for p in paths)
    assert any(str(p).endswith("fff/master.sock") for p in paths)


def test_import_pulls_in_no_third_party_packages():
    """Importing fff_client must require only the standard library.

    Snapshot ``sys.modules`` before/after a fresh import of the package and its
    submodules, then assert every newly-imported top-level module is part of the
    stdlib. This is what makes the package safe to import under Python 3.9 with
    nothing installed.
    """
    for name in list(sys.modules):
        if name == "fff_client" or name.startswith("fff_client."):
            del sys.modules[name]

    before = set(sys.modules)
    import fff_client  # noqa: F401
    import fff_client._client  # noqa: F401
    import fff_client._paths  # noqa: F401
    import fff_client._protocol  # noqa: F401

    new_top_level = {
        name.split(".", 1)[0]
        for name in set(sys.modules) - before
        if not name.startswith("fff_client")
    }

    stdlib = set(getattr(sys, "stdlib_module_names", ()))
    assert stdlib, "sys.stdlib_module_names unavailable; cannot assert stdlib-only"

    third_party = {m for m in new_top_level if m and not m.startswith("_") and m not in stdlib}
    assert not third_party, "fff_client imported non-stdlib modules: {}".format(
        sorted(third_party)
    )
