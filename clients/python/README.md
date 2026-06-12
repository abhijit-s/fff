# fff-client

A thin, **stdlib-only** Python client for fff's **engine daemon** — the warm,
shared, frecency-ranked surface the fff MCP (Model Context Protocol) tools use.
It speaks fff's versioned JSON wire protocol over the daemon's Unix sockets and
**fails loud on protocol skew**.

- Pure Python, no third-party runtime dependencies (`socket`, `json`, `struct`,
  `pathlib`, `os`, `typing`).
- `requires-python >= 3.9`.
- **Connect-and-fail:** never spawns a daemon. If the engine is not already
  running, you get a clean `FffUnreachable` rather than a hang.

## Install

```sh
pip install ./clients/python   # from the fff repo root
```

## Usage

```python
from fff_client import FffClient

with FffClient(base_path="/path/to/repo") as fff:
    # Fuzzy file search — list of {path, score, git_status, frecency_score}
    for hit in fff.find_files("main", limit=10):
        print(hit["path"], hit["score"], hit["frecency_score"])

    # Content search — WireGrepResponse dict
    resp = fff.grep("TODO", page_limit=20)
    for file_match in resp["matches"]:
        for m in file_match["matches"]:
            print(file_match["path"], m["line_number"], m["line_text"])

    # OR-search across patterns, with a constraint string
    fff.multi_grep(["FffClient", "fff_client"], constraints="*.py")

    # Enumerate targetable roots
    fff.list_roots()                       # [{base_path, name, default}, …]
    fff.list_recent_files(limit=20, dirty_only=True)
    fff.get_git_status(include_clean=False)
    fff.health()
```

### record_access (opt-in, default OFF)

Recording access pollutes the human frecency signal, so it is gated behind an
explicit flag and is a no-op otherwise:

```python
with FffClient(base_path="/repo", record_access_enabled=True) as fff:
    fff.record_access("/repo/src/main.rs")  # fire-and-forget, no reply read
```

### Socket resolution

By default the client resolves the master socket the same way the engine does
(`$XDG_CACHE_HOME` → `$HOME/.cache` → platform cache dir → `/tmp`, then
`fff/master.sock`). Override with the `FFF_MASTER_SOCK` env var or the
`master_sock=` constructor argument.

## Errors

- `FffUnreachable` — daemon socket absent or refusing (subclass of `FffError`).
- `FffError(code, message)` — typed engine error; `code="PROTOCOL_MISMATCH"`
  on version skew.

## Compatibility

The wire version is pinned in `PROTOCOL_VERSION` and must match the engine's
`fff_ipc::PROTOCOL_VERSION` exactly; a mismatch raises
`FffError(code="PROTOCOL_MISMATCH")` and returns **no** partial results.

| `fff-client` | `PROTOCOL_VERSION` | Compatible engine                                  |
| ------------ | ------------------ | -------------------------------------------------- |
| 0.1.x        | 1                  | Any `fff-engine` whose `fff_ipc::PROTOCOL_VERSION` == 1 |

A clean `PROTOCOL_MISMATCH` requires an engine new enough to speak the JSON
envelope. Against an older engine (pre-versioned protocol, JSON-unaware) the
connection simply fails rather than returning a typed mismatch — ship the engine
and client together.
