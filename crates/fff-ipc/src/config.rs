use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Shared configuration for fff-engine and fff-mcp.
///
/// Loaded from `$XDG_CONFIG_HOME/fff/config.toml`, falling back to
/// `$HOME/.config/fff/config.toml`. CLI flags always override config values.
///
/// Example config file:
/// ```toml
/// [log]
/// level = "debug"
/// # file = "~/.cache/fff_engine.log"  # defaults to ~/.cache/fff_{binary}.log
///
/// [index]
/// no_watch = false
/// no_warmup = false
/// max_cached_files = 30000
/// idle_root_ttl_secs = 21600  # evict on-demand roots unqueried this long
/// #                             (0 = no idle eviction; deleted-worktree roots still reaped)
///
/// [frecency]
/// # db = "~/.local/share/fff/frecency/"  # set to share one DB across projects
///
/// [mcp]
/// default = "app"   # root used when a tool call omits base_path (name or path)
///
/// [[mcp.roots]]
/// name = "app"
/// path = "/Users/you/work/app"
/// ```
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FffConfig {
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub frecency: FrecencyConfig,
    #[serde(default)]
    pub worker: WorkerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

/// Project roots fff-mcp searches. Ignored by fff-engine and fffctl.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    /// Default root selector: a declared name or an absolute path.
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub roots: Vec<McpRoot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRoot {
    pub path: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    /// Gitignore-style glob patterns excluded from this root's index and
    /// watcher (e.g. `["target/", "**/*.log", "!keep/"]`). Empty by default.
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("duplicate root name {0:?} in [mcp].roots")]
    DuplicateName(String),
    #[error("[mcp].default {0:?} matched no root name and is not an absolute path")]
    UnresolvedDefault(String),
}

impl McpConfig {
    /// Reject duplicate root names and a `default` that resolves to nothing.
    pub fn validate(&self) -> Result<(), McpConfigError> {
        let mut seen: Vec<&str> = Vec::new();
        for name in self.roots.iter().filter_map(|r| r.name.as_deref()) {
            if seen.contains(&name) {
                return Err(McpConfigError::DuplicateName(name.to_string()));
            }
            seen.push(name);
        }
        if let Some(def) = self.default.as_deref() {
            let known = self.roots.iter().any(|r| r.name.as_deref() == Some(def));
            if !known && !Path::new(def).is_absolute() {
                return Err(McpConfigError::UnresolvedDefault(def.to_string()));
            }
        }
        Ok(())
    }

    /// Gitignore-style ignore patterns for the registered root that best
    /// contains `base_path` (canonical longest-prefix match — same identity
    /// discipline as name resolution). Empty when no configured root matches.
    pub fn ignore_for(&self, base_path: &Path) -> Vec<String> {
        let target = std::fs::canonicalize(base_path).unwrap_or_else(|_| base_path.to_path_buf());
        let mut best: Option<&McpRoot> = None;
        let mut best_len = 0usize;
        for root in &self.roots {
            if root.ignore.is_empty() {
                continue;
            }
            let rp = std::fs::canonicalize(&root.path).unwrap_or_else(|_| root.path.clone());
            if target.starts_with(&rp) && rp.as_os_str().len() > best_len {
                best_len = rp.as_os_str().len();
                best = Some(root);
            }
        }
        best.map(|r| r.ignore.clone()).unwrap_or_default()
    }

    /// Resolve `default` to a concrete path (name → declared path, or an
    /// absolute path as-is). `None` when no default is set.
    pub fn default_path(&self) -> Option<PathBuf> {
        let def = self.default.as_deref()?;
        if let Some(r) = self.roots.iter().find(|r| r.name.as_deref() == Some(def)) {
            return Some(r.path.clone());
        }
        Path::new(def).is_absolute().then(|| PathBuf::from(def))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// Minimum number of worker processes to keep alive.
    pub n_min: u32,
    /// Maximum number of worker processes to spawn.
    pub n_max: u32,
    /// Maximum roots loaded per worker before a new worker is spawned.
    pub roots_per_worker_max: u32,
    /// Seconds a worker with no loaded roots waits before being shut down.
    pub idle_ttl_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            n_min: 1,
            n_max: 4,
            roots_per_worker_max: 8,
            idle_ttl_secs: 300,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LogConfig {
    /// Log level: trace, debug, info, warn, error. Default: info.
    pub level: String,
    /// Override the log file path. Default: `~/.cache/fff_{binary}.log`.
    pub file: Option<String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            // Target our crates at info; suppress library noise at warn.
            // Use RUST_LOG-style syntax for finer control:
            //   "fff_engine=debug,fff_mcp=debug,warn"
            level: "fff_engine=info,fff_mcp=info,warn".into(),
            file: None,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Disable the background filesystem watcher.
    #[serde(default)]
    pub no_watch: bool,
    /// Skip mmap warmup after initial scan.
    #[serde(default)]
    pub no_warmup: bool,
    /// Maximum number of files to keep content-cached in memory.
    pub max_cached_files: Option<usize>,
    /// Seconds an on-demand root may go unqueried before the master evicts it
    /// (reclaiming its worker slot and watcher). Unset ⇒ 21600 (6h). `0`
    /// disables idle-age eviction only — deleted-directory / dangling-worktree
    /// roots are still reaped. Env override: `FFF_IDLE_ROOT_TTL_SECS`. Configured
    /// `[[mcp.roots]]` are always exempt.
    pub idle_root_ttl_secs: Option<u64>,
}

impl IndexConfig {
    pub const DEFAULT_IDLE_ROOT_TTL_SECS: u64 = 21600;

    /// Resolve the idle-root TTL: `FFF_IDLE_ROOT_TTL_SECS` env override wins,
    /// then the config value, then the 6h default. `0` means disabled.
    pub fn resolved_idle_root_ttl_secs(&self) -> u64 {
        // A set-but-unparseable env override is a user mistake worth surfacing
        // rather than silently discarding. eprintln! (not tracing) mirrors the
        // rest of this module — tracing may not be initialised yet.
        if let Ok(raw) = std::env::var("FFF_IDLE_ROOT_TTL_SECS") {
            match raw.parse() {
                Ok(secs) => return secs,
                Err(_) => {
                    eprintln!("Warning: FFF_IDLE_ROOT_TTL_SECS={raw:?} is not a valid u64; ignoring")
                }
            }
        }
        self.idle_root_ttl_secs
            .unwrap_or(Self::DEFAULT_IDLE_ROOT_TTL_SECS)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FrecencyConfig {
    /// Path to the LMDB frecency database directory.
    /// Default: a per-base-path subdirectory under
    /// `$XDG_DATA_HOME/fff/frecency/<slug>/` (the slug is a stable hash of
    /// the canonical base-path). Set this to a fixed directory to share one
    /// DB across all projects — useful when you want cross-project frecency
    /// signal, at the cost of a global size-cap blast radius.
    pub db: Option<String>,
}

/// Returns the config file path:
/// `$XDG_CONFIG_HOME/fff/config.toml` or `$HOME/.config/fff/config.toml`.
///
/// Does not use `dirs::config_dir()` — that returns `~/Library/Application Support`
/// on macOS instead of the XDG-conventional `~/.config`.
pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(
                std::env::var("HOME")
                    .or_else(|_| std::env::var("USERPROFILE"))
                    .unwrap_or_else(|_| "/tmp".to_string()),
            )
            .join(".config")
        });
    base.join("fff").join("config.toml")
}

/// Load the config from the default XDG path. Returns `FffConfig::default()`
/// when the file is absent or unreadable/unparseable, warning to stderr
/// (tracing may not be initialised yet). The implicit path is best-effort;
/// for an explicitly-named file use [`load_from`], which errors instead.
pub fn load() -> FffConfig {
    let path = config_path();
    if !path.exists() {
        return FffConfig::default();
    }
    load_from(&path).unwrap_or_else(|e| {
        eprintln!("Warning: {e}");
        FffConfig::default()
    })
}

/// Load the config from an explicit path. Unlike [`load`], read/parse failures
/// are returned as errors — the caller named this file, so silently falling
/// back to defaults would hide their mistake.
pub fn load_from(path: &Path) -> Result<FffConfig, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read fff config at {}: {e}", path.display()))?;
    toml::from_str(&contents)
        .map_err(|e| format!("failed to parse fff config at {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_config_defaults() {
        let c = WorkerConfig::default();
        assert_eq!(c.n_min, 1);
        assert_eq!(c.n_max, 4);
        assert_eq!(c.roots_per_worker_max, 8);
        assert_eq!(c.idle_ttl_secs, 300);
    }

    #[test]
    fn index_idle_root_ttl_parses_and_defaults() {
        let cfg: FffConfig = toml::from_str("[index]\nidle_root_ttl_secs = 600\n").unwrap();
        assert_eq!(cfg.index.idle_root_ttl_secs, Some(600));

        let bare: FffConfig = toml::from_str("[index]\nno_watch = true\n").unwrap();
        assert_eq!(bare.index.idle_root_ttl_secs, None);
    }

    #[test]
    fn resolved_idle_root_ttl_uses_default_when_unset() {
        // No env override in this test process ⇒ config value, then the 6h default.
        let unset = IndexConfig::default();
        assert_eq!(
            unset.resolved_idle_root_ttl_secs(),
            IndexConfig::DEFAULT_IDLE_ROOT_TTL_SECS
        );
        let set = IndexConfig {
            idle_root_ttl_secs: Some(0),
            ..Default::default()
        };
        assert_eq!(set.resolved_idle_root_ttl_secs(), 0);
    }

    #[test]
    fn fff_config_without_worker_section_uses_defaults() {
        let toml = "[log]\nlevel = \"debug\"\n";
        let cfg: FffConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.worker.n_min, 1);
        assert_eq!(cfg.worker.n_max, 4);
    }

    #[test]
    fn fff_config_with_worker_section_parses_fields() {
        let toml =
            "[worker]\nn_min = 2\nn_max = 8\nroots_per_worker_max = 16\nidle_ttl_secs = 600\n";
        let cfg: FffConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.worker.n_min, 2);
        assert_eq!(cfg.worker.n_max, 8);
        assert_eq!(cfg.worker.roots_per_worker_max, 16);
        assert_eq!(cfg.worker.idle_ttl_secs, 600);
    }

    #[test]
    fn mcp_section_parses_roots_and_default() {
        let toml = r#"
[mcp]
default = "fff"

[[mcp.roots]]
name = "fff"
path = "/tmp/fff"

[[mcp.roots]]
path = "/tmp/anon"
"#;
        let cfg: FffConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.mcp.roots.len(), 2);
        assert_eq!(cfg.mcp.default.as_deref(), Some("fff"));
        cfg.mcp.validate().unwrap();
        assert_eq!(cfg.mcp.default_path(), Some(PathBuf::from("/tmp/fff")));
    }

    #[test]
    fn mcp_default_as_absolute_path_resolves() {
        let cfg: McpConfig = toml::from_str("default = \"/tmp/x\"\n").unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.default_path(), Some(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn mcp_missing_section_is_empty() {
        let cfg: FffConfig = toml::from_str("[log]\nlevel = \"info\"\n").unwrap();
        assert!(cfg.mcp.roots.is_empty());
        assert!(cfg.mcp.default.is_none());
    }

    #[test]
    fn mcp_duplicate_names_rejected() {
        let cfg: McpConfig = toml::from_str(
            "[[roots]]\nname = \"a\"\npath = \"/tmp/a\"\n\n[[roots]]\nname = \"a\"\npath = \"/tmp/b\"\n",
        )
        .unwrap();
        match cfg.validate().unwrap_err() {
            McpConfigError::DuplicateName(n) => assert_eq!(n, "a"),
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    #[test]
    fn mcp_default_unknown_name_rejected() {
        let cfg: McpConfig = toml::from_str(
            "default = \"ghost\"\n\n[[roots]]\nname = \"real\"\npath = \"/tmp/real\"\n",
        )
        .unwrap();
        match cfg.validate().unwrap_err() {
            McpConfigError::UnresolvedDefault(v) => assert_eq!(v, "ghost"),
            other => panic!("expected UnresolvedDefault, got {other:?}"),
        }
    }

    #[test]
    fn mcp_root_ignore_parses_and_defaults_empty() {
        let toml = r#"
[[mcp.roots]]
path = "/tmp/proj"
ignore = ["target/", "**/*.log"]

[[mcp.roots]]
path = "/tmp/other"
"#;
        let cfg: FffConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.mcp.roots[0].ignore, vec!["target/", "**/*.log"]);
        assert!(
            cfg.mcp.roots[1].ignore.is_empty(),
            "ignore defaults to empty when omitted"
        );
    }

    #[test]
    fn mcp_ignore_for_matches_containing_root() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("proj");
        let child = parent.join("src").join("deep");
        std::fs::create_dir_all(&child).unwrap();

        let mut cfg = McpConfig::default();
        cfg.roots.push(McpRoot {
            path: parent.clone(),
            name: None,
            ignore: vec!["target/".into()],
        });

        // A sub-path resolves to the containing root's patterns.
        assert_eq!(cfg.ignore_for(&child), vec!["target/".to_string()]);
        // An unrelated path matches nothing.
        assert!(
            cfg.ignore_for(Path::new("/nonexistent/unrelated"))
                .is_empty()
        );
    }
}
