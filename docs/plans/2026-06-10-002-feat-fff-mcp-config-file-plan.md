# feat(fff-mcp): `--config` flag for unified root registry

Date: 2026-06-10
Branch: `feat/fff-mcp-config-file`
Status: implementing

## Goal

Add `--config <PATH>` to `fff-mcp` that loads a TOML file describing roots and
the default root. The config file unifies the asymmetric `--base-path` and
`--root` shapes into one model and adds **named roots** so tool callers can
pass either a path or a registered name as `base_path`.

## Conceptual model

A **root** = one indexed scope.

```
Root { path: PathBuf, name: Option<String> }
```

One root in the registry may be the **default** (used when the tool call omits
`base_path`).

## TOML schema

```toml
# Optional: name or absolute path of the default root.
default = "fff"

[[roots]]
name = "fff"
path = "/Users/a.salvi/my-workspace/util/fff"

[[roots]]
name = "turbovec"
path = "/Users/a.salvi/my-workspace/ai/turbovec"

[[roots]]
# name optional
path = "/Users/a.salvi/.dotfiles/ai"
```

Validation rules:

- Unknown top-level keys reject parse (serde `deny_unknown_fields`).
- `default` (when present) must be either an absolute path **or** match a
  registered `name`. Otherwise startup fails.
- `path` is required on every `[[roots]]` entry.
- `default` need not be set — caller can rely on CLI/cwd fallback.

## Behavior

1. `--config <PATH>` loads the TOML at startup. Parse errors fail fast with a
   message mentioning the file path.
2. Tool calls with `base_path` matching a registered `name` are resolved to
   the root's `path` **before** canonicalization / pool routing.
3. Tool calls with `base_path` that is a real path keep working.
4. `list_roots` output extends with `name: string | null` (omitted entirely
   when not set, via `skip_serializing_if`).
5. Precedence (high → low): CLI `--base-path` > config `default` > cwd.
   Additional `--root` flags append to (and dedupe with) config `[[roots]]`.
6. Existing single-root invocations (no `--config`, no `--root`) are
   byte-identical to `main`.

## Files to touch

- `crates/fff-mcp/Cargo.toml` — add `toml = "0.8"` (parse feature only).
- `crates/fff-mcp/src/registry.rs`
  - extend `RootRegistry` to carry `name: Option<String>` per root
  - add `resolve_name(name) -> Option<&Path>` lookup
  - add `all_with_names()` for `list_roots`
  - keep existing `RootRegistry::new(default, extras)` API (used by direct
    code paths and integration tests). New constructor
    `RootRegistry::with_named(default, named_extras)` for config-driven
    construction.
  - parser: `ConfigFile::load(path)` returning `(roots, default_hint)`.
- `crates/fff-mcp/src/main.rs`
  - add `--config` clap flag
  - load the file (when present), merge with CLI args, build registry.
- `crates/fff-mcp/src/server.rs`
  - `resolve_route`: if the explicit `base_path` is **not** a default-equivalent
    path, first ask the registry to interpret it as a name; if matched, swap
    to that root's path before continuing.
  - `list_roots`: include `name` field when set.
  - update `list_roots` tool description to mention names and the `name`
    field in the response.
- `crates/fff-mcp/tests/integration.rs` — integration tests for config loading
  & precedence.
- Inline `#[cfg(test)]` in `registry.rs` — unit tests for parsing, validation,
  name resolution, dedupe with extras.

## Test scenarios

Registry unit tests (`registry.rs`):

1. `ConfigFile::load` parses a valid TOML.
2. Parse error mentions the offending path.
3. `default` matching a registered name resolves correctly.
4. `default` set to an absolute path also works (no name required).
5. `default` set to an unknown name → error.
6. Unknown keys in TOML rejected.
7. `RootRegistry::resolve_name` returns the right path; non-name returns None.
8. Name-collisions are tolerated by keeping the first occurrence (or
   deterministic; we'll pick: error on duplicate names — cheap to check).
9. Additional `--root` flags merge & dedupe by canonical path with config
   roots.
10. `list_roots` `(name, path, is_default)` triples are stable.

Integration tests (`integration.rs`):

11. Tool call with `base_path = "<name>"` resolves to the registered root.
12. Tool call with `base_path = "<unknown-name>"` returns a clear error
    (not silently routed to cwd).

## Out of scope (per spec)

- Auto-discovery of `$XDG_CONFIG_HOME/fff/mcp.toml`.
- Watching the config file for changes.
- Validating that paths exist on disk.

## Design decisions

- **Errors at startup propagate as boxed errors from `main`** (`Box<dyn Error>`).
  Message format: `failed to load --config <PATH>: <toml parse error>`.
- **`name`** in `list_roots` JSON is **omitted** when unset (cleaner than
  `null`, and serde `skip_serializing_if` makes it trivial).
- **Duplicate names** rejected at startup as user-error.
- **Name lookup happens once** at `resolve_route` entry — name → path swap is
  a pure registry method, no side effects.
- **Backward compatibility**: existing `RootRegistry::new(default, Vec<PathBuf>)`
  retained verbatim. Config path goes through a new builder.

## Commit plan

Single commit on `feat/fff-mcp-config-file`:

```
feat(fff-mcp): add --config flag for unified root registry

TOML config file lets users declare default + additional roots
with optional names. Tool calls can pass either a registered
name or an absolute path as base_path. CLI --base-path and
--root remain available and override config values; existing
single-root invocations are byte-identical.
```

Incremental local commits per the agent guidance, squashed at the end with
`git reset --soft main`. No push, no PR.
