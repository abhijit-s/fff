"""Smoke test: the package imports stdlib-only and exports its public surface.

The full round-trip / fake-engine / version-skew / path-parity matrix is U7.
"""


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
