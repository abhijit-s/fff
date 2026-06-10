# Unify fff-mcp roots into the single `config.toml`

**Status:** Draft — awaiting approval
**Date:** 2026-06-10
**Author:** plan via Claude

## Problem

Two config files live side by side in `~/.config/fff/` and are constantly confused:

| File | Schema | Loaded by | Purpose |
|------|--------|-----------|---------|
| `config.toml` | `FffConfig` (`fff-ipc`) | auto, fixed XDG path | daemon: log, index, frecency, worker |
| `roots.toml` | `ConfigFile` (`fff-mcp/registry.rs`) | only via `--config <path>` | which project roots to search |

The trap: the `--config` flag loads the **roots** file, while the file literally named `config.toml` auto-loads and is something else. Same directory, same vibe, never touch. A stale doc comment (`crates/fff-mcp/src/main.rs:134`) even references a non-existent `--base-path` flag, which is what produced a broken MCP registration in the first place.

## Goal

One auto-loaded config file. Roots become an `[mcp]` section of `config.toml`. The common-case MCP registration needs **no flag**.

```toml
[log]
level = "fff_engine=info,fff_mcp=info,warn"

[mcp]
default = "abhi"            # a declared name or an absolute path

[[mcp.roots]]
name = "abhi"
path = "/Users/a.salvi/Work/abhi.easygo.io"

[[mcp.roots]]
name = "fff"
path = "/Users/a.salvi/my-workspace/util/fff"
```

```bash
claude mcp add -s user fff -- $(brew --prefix)/bin/fff-mcp   # no flag
```

## Non-goals

- No change to the daemon (`fff-engine`) or `fffctl`. They deserialize `FffConfig` and will ignore the new `[mcp]` table.
- No change to top-level Rust/Lua/C/Bun APIs (frozen per CLAUDE.md).
- Positional `[PATH]` and `--root <PATH>` flags remain as overrides.

## Design

### 1. Schema — `crates/fff-ipc/src/config.rs`

Add an `mcp` section to `FffConfig`:

```rust
#[serde(default)]
pub mcp: McpConfig,
```

New public types (replacing `registry::ConfigFile` / `ConfigRoot`):

```rust
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub default: Option<String>,   // declared name or absolute path
    #[serde(default)]
    pub roots: Vec<McpRoot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRoot {
    pub path: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
}
```

Port helpers, **split by crate boundary** (keeps MCP-registry logic out of the shared daemon crate, per CLAUDE.md "no public structs if it can be private"):
- On `McpConfig` in `fff-ipc` (pure config logic): `default_path(&self) -> Option<PathBuf>` (resolve `default`: name → path, or accept absolute path) and `validate(&self) -> Result<(), McpConfigError>` (duplicate-name + unresolved-default checks, currently `ConfigError::DuplicateName` / `UnresolvedDefault`).
- In `fff-mcp` (private, operates on the now-public `&McpConfig`): `name_for_path` — it exists only to feed `build_registry`'s `default_name`. Fold its few lines into `build_registry` or keep as a private helper there.

**Fix a latent bug while porting `name_for_path`.** Today it does literal `r.path == path`, but `build_registry` calls it with the git-discovered base_path (main.rs:341–359 rewrites base_path to the git workdir, possibly canonicalized/symlink-resolved). A literal `==` then silently fails and the default root loses its name. Canonicalize both sides:
```rust
fn name_for_path(cfg: &McpConfig, path: &Path) -> Option<String> {
    let target = canonicalize(path);
    cfg.roots.iter()
        .find(|r| canonicalize(&r.path) == target)
        .and_then(|r| r.name.clone())
}
```
`default_path` stays literal — its output flows into `RootRegistry::with_named`, which already canonicalizes. Don't add canonicalization asymmetrically.

Keep `FffConfig` lenient (no `deny_unknown_fields`) so engine/ctl tolerate it. Verified inert: `fff-engine` master extracts only `config.worker` to pass to workers (master.rs:407); ctl unaffected.

### 2. Startup — `crates/fff-mcp/src/main.rs`

- **`--config` semantics change**: from "roots file" → "path to a `config.toml`-format file, overriding the default XDG path." Update `Args` field + help text. Keep the flag name `--config`.
- Add `fff_ipc::config::load_from(path: &Path) -> FffConfig`; refactor `load()` to call it with `config_path()`.
- **Resolve which config to load FIRST, before `resolve_defaults` (do this as the first edit in this step).** `load()` runs unconditionally at main.rs:272 and feeds `resolve_defaults` at 273 — if `--config` is loaded later (old lines 314–323), `resolve_defaults` reads the XDG config while roots read the override, a split-brain bug. Instead:
  ```rust
  let cfg = match args.config_file.as_deref() {
      Some(path) => fff_ipc::config::load_from(path),
      None => fff_ipc::config::load(),
  };
  resolve_defaults(&mut args, &cfg);
  ```
  Everything downstream reads this one `cfg`. Delete the separate load block at 314–323 entirely.
- **Explicit `--config` load failures are fatal.** `load()` swallows read/parse errors → `default()` (config.rs:130), which is right for the implicit XDG path but wrong when the user explicitly named a file. `load_from` (or the `--config` branch) must `eprintln!` + `exit(1)` on read/parse failure, matching today's hard-erroring `ConfigFile::load`.
- After loading `cfg`, call `cfg.mcp.validate()`; on error `eprintln!` + `exit(1)`.
- base_path resolution: CLI positional > `cfg.mcp.default_path()` > cwd (replaces `config_file.resolve_default_path()` at lines 325–339).
- `build_registry(base_path, &args.root, &cfg.mcp)` (replaces `Option<&ConfigFile>`). Only the Unix proxy path (line 370) uses it; the direct/fallback path is unchanged.

### 3. Remove dead schema — `crates/fff-mcp/src/registry.rs`

Delete `ConfigFile`, `ConfigRoot`, `ConfigError`, and their `load`/`resolve_default_path`/`name_for_path` impls. `RootRegistry` + `with_named` + `canonicalize` stay unchanged. Update `build_registry` to read from `&McpConfig`. After deletion the file holds a single impl — no split needed. **Also update the module doc-comment (registry.rs:1–6)**, which still references `--base-path` and "a `--config` TOML file may also declare roots."

### 4. Migration safety (minimal)

No bespoke raw-TOML guard — only the author has the old format, migrated by hand in step 5 (the "error handling for scenarios that can't happen" anti-pattern). Reuse the already-parsed `cfg`: **when `--config` is passed and `cfg.mcp.roots` is empty, `eprintln!` one warning** (catches the realistic mistake of pointing `--config` at an old `roots.toml`). A CHANGELOG line covers the rest.

### 5. User's machine migration (post-merge, manual step)

1. Rewrite `~/.config/fff/config.toml` to add the `[mcp]` section (abhi/fff/turbovec, default `abhi`).
2. Delete `~/.config/fff/roots.toml`.
3. `brew reinstall --HEAD abhijit-s/fff/fff`.
4. Re-register: `claude mcp add -s user fff -- $(brew --prefix)/bin/fff-mcp` (no flag).
5. Verify `claude mcp list` → `✔ Connected`.

### 6. Docs

- `README.md`: rewrite the "Multiple project roots" subsection to the `[mcp]` model + bare registration. Note `--config` now means "alternate config.toml path."
- `crates/fff-ipc/src/config.rs`: extend the doc-comment example with `[mcp]`.
- `Formula/fff.rb`: add `[mcp]` to the caveats config example.
- Fix the stale `--base-path` comment at `main.rs:134` (rewritten in this work anyway).
- `CHANGELOG.md` if present: note the `--config` semantic change.

### 7. Tests

- Move `ConfigFile` parse tests → `fff-ipc` config tests: parse `[mcp]` roots; default-by-name; default-by-absolute-path; duplicate-name error; unresolved-default error.
- Keep `RootRegistry::with_named` tests in `registry.rs`.
- Adapt the `build_registry` test to `McpConfig`.

### 8. Verify

`make build && make lint && make test`, then the manual MCP reconnect from step 5.

## Back-compat / blast radius

- **Breaking for the days-old `--config roots.toml` form** — only the author uses it; handled by migration. Warrants a minor version bump (0.13.2 → 0.14.0) and a CHANGELOG entry. Publish is a separate `/publish` step.
- Engine/ctl: zero behavior change (ignore `[mcp]`).
- Positional `[PATH]` + `--root`: unchanged.

## Sequence

1. Schema + helpers in `fff-ipc` (+ tests).
2. Rewire `main.rs`; delete dead schema in `registry.rs`.
3. Migration guard.
4. Build + lint + test.
5. Docs.
6. Migrate user's machine; reconnect; verify.
7. Branch + PR.
