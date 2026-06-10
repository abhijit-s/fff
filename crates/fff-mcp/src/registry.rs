//! Startup registry of project roots fff-mcp can search.
//!
//! `--base-path` sets the default; `--root` flags add additional roots.
//! A `--config` TOML file may also declare roots (optionally named) and a
//! default. All paths are canonicalized once at construction so downstream
//! lookups (pool, longest-prefix match) compare against absolute paths.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Clone, Debug)]
struct RootEntry {
    name: Option<String>,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RootRegistry {
    default: PathBuf,
    /// Name of the default root, when one was declared in the config file.
    default_name: Option<String>,
    additional: Vec<RootEntry>,
}

impl RootRegistry {
    /// Build a registry from CLI args. `default` is the value of `--base-path`
    /// (or the discovered git root); `extras` are the `--root` values.
    /// All entries are canonicalized; duplicates against the default and
    /// across `extras` are silently dropped.
    #[allow(dead_code)] // exercised by integration tests + downstream callers
    pub fn new<P: AsRef<Path>>(default: P, extras: impl IntoIterator<Item = PathBuf>) -> Self {
        Self::with_named(default, None, extras.into_iter().map(|p| (None, p)))
    }

    /// Build a registry with optional names per extra root.
    ///
    /// `default_name` is the name of the default root (when known — e.g. when
    /// the config file's `default` selected a named root). Extras are
    /// canonicalized; duplicates by canonical path are dropped (first wins).
    pub fn with_named<P: AsRef<Path>>(
        default: P,
        default_name: Option<String>,
        extras: impl IntoIterator<Item = (Option<String>, PathBuf)>,
    ) -> Self {
        let default = canonicalize(default.as_ref());
        let mut additional: Vec<RootEntry> = Vec::new();
        for (name, raw) in extras {
            let canon = canonicalize(&raw);
            if canon == default {
                continue;
            }
            if additional.iter().any(|e| e.path == canon) {
                continue;
            }
            additional.push(RootEntry { name, path: canon });
        }
        Self {
            default,
            default_name,
            additional,
        }
    }

    pub fn default_root(&self) -> &Path {
        &self.default
    }

    #[allow(dead_code)]
    pub fn default_name(&self) -> Option<&str> {
        self.default_name.as_deref()
    }

    /// Resolve a `base_path` argument as a registered name. Returns the
    /// canonicalized path when a name matches, `None` otherwise.
    pub fn resolve_name(&self, name: &str) -> Option<&Path> {
        if self.default_name.as_deref().is_some_and(|n| n == name) {
            return Some(self.default.as_path());
        }
        self.additional
            .iter()
            .find(|e| e.name.as_deref() == Some(name))
            .map(|e| e.path.as_path())
    }

    /// All registered roots, default first. Returns `(path, is_default)` pairs
    /// suitable for the original `list_roots` MCP response.
    #[allow(dead_code)] // exercised by integration tests
    pub fn all(&self) -> Vec<(&Path, bool)> {
        let mut out: Vec<(&Path, bool)> = Vec::with_capacity(1 + self.additional.len());
        out.push((self.default.as_path(), true));
        for e in &self.additional {
            out.push((e.path.as_path(), false));
        }
        out
    }

    /// All registered roots with names, default first.
    pub fn all_with_names(&self) -> Vec<(Option<&str>, &Path, bool)> {
        let mut out: Vec<(Option<&str>, &Path, bool)> =
            Vec::with_capacity(1 + self.additional.len());
        out.push((self.default_name.as_deref(), self.default.as_path(), true));
        for e in &self.additional {
            out.push((e.name.as_deref(), e.path.as_path(), false));
        }
        out
    }

    /// Longest-prefix match for a given file path. Returns the registered
    /// root that owns the file, falling back to the default if none match.
    pub fn root_for_path(&self, path: &Path) -> PathBuf {
        let target = canonicalize(path);
        let mut best: Option<&Path> = None;
        let mut best_len: usize = 0;
        for root in std::iter::once(self.default.as_path())
            .chain(self.additional.iter().map(|e| e.path.as_path()))
        {
            if target.starts_with(root) {
                let len = root.as_os_str().len();
                if len > best_len {
                    best_len = len;
                    best = Some(root);
                }
            }
        }
        best.map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.default.clone())
    }

    /// True when `base_path` (canonicalized) equals the default root.
    pub fn is_default(&self, base_path: &Path) -> bool {
        canonicalize(base_path) == self.default
    }
}

fn canonicalize(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── Config file ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Default root selector — either a registered name or an absolute path.
    #[serde(default)]
    pub default: Option<String>,
    /// Roots declared in the file.
    #[serde(default)]
    pub roots: Vec<ConfigRoot>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRoot {
    pub path: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("duplicate root name {name:?} in {path}")]
    DuplicateName { path: PathBuf, name: String },
    #[error(
        "default {value:?} in {path} did not match any registered name (and is not an absolute path)"
    )]
    UnresolvedDefault { path: PathBuf, value: String },
}

impl ConfigFile {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg: ConfigFile = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        cfg.validate(path)?;
        Ok(cfg)
    }

    fn validate(&self, source_path: &Path) -> Result<(), ConfigError> {
        let mut seen: Vec<&str> = Vec::new();
        for r in &self.roots {
            if let Some(name) = r.name.as_deref() {
                if seen.contains(&name) {
                    return Err(ConfigError::DuplicateName {
                        path: source_path.to_path_buf(),
                        name: name.to_string(),
                    });
                }
                seen.push(name);
            }
        }
        if let Some(def) = self.default.as_deref() {
            let matches_name = self.roots.iter().any(|r| r.name.as_deref() == Some(def));
            let looks_like_path = Path::new(def).is_absolute();
            if !matches_name && !looks_like_path {
                return Err(ConfigError::UnresolvedDefault {
                    path: source_path.to_path_buf(),
                    value: def.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Resolve the config's `default` field to a concrete path, using the
    /// declared roots to interpret names. Returns `None` when no default is
    /// set in the file.
    pub fn resolve_default_path(&self) -> Option<PathBuf> {
        let def = self.default.as_deref()?;
        if let Some(by_name) = self.roots.iter().find(|r| r.name.as_deref() == Some(def)) {
            return Some(by_name.path.clone());
        }
        if Path::new(def).is_absolute() {
            return Some(PathBuf::from(def));
        }
        None
    }

    /// Find the name (if any) attached to the config root whose path equals
    /// `path` after literal comparison.
    pub fn name_for_path(&self, path: &Path) -> Option<String> {
        self.roots
            .iter()
            .find(|r| r.path == path)
            .and_then(|r| r.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn registry_default_only_when_no_extra_roots() {
        let tmp = TempDir::new().unwrap();
        let reg = RootRegistry::new(tmp.path(), Vec::<PathBuf>::new());
        let all = reg.all();
        assert_eq!(all.len(), 1);
        assert!(all[0].1, "the one entry must be the default");
    }

    #[test]
    fn registry_dedupes_extra_roots_against_default() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let reg = RootRegistry::new(&a, vec![a.clone(), b.clone(), b.clone()]);
        let all = reg.all();
        assert_eq!(all.len(), 2);
        assert!(all[0].1);
        assert!(!all[1].1);
        assert_eq!(all[1].0, b.canonicalize().unwrap());
    }

    #[test]
    fn registry_canonicalizes_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = RootRegistry::new(&a, vec![]);
        assert!(reg.default_root().is_absolute());
        assert_eq!(reg.default_root(), a.canonicalize().unwrap());
    }

    #[test]
    fn registry_all_returns_default_first() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let c = tmp.path().join("c");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        let reg = RootRegistry::new(&a, vec![b.clone(), c.clone()]);
        let all = reg.all();
        assert_eq!(all.len(), 3);
        assert!(all[0].1);
        assert_eq!(all[0].0, a.canonicalize().unwrap());
        assert_eq!(all[1].0, b.canonicalize().unwrap());
        assert_eq!(all[2].0, c.canonicalize().unwrap());
    }

    #[test]
    fn root_for_path_under_registered_root() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("repo_a");
        let b = tmp.path().join("repo_b");
        std::fs::create_dir_all(a.join("src")).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let reg = RootRegistry::new(&a, vec![b.clone()]);
        let inside = a.join("src");
        let got = reg.root_for_path(&inside);
        assert_eq!(got, a.canonicalize().unwrap());
    }

    #[test]
    fn root_for_path_longest_prefix_wins() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path().join("repo");
        let inner = outer.join("inner");
        std::fs::create_dir_all(inner.join("x")).unwrap();
        let reg = RootRegistry::new(&outer, vec![inner.clone()]);
        let probe = inner.join("x");
        let got = reg.root_for_path(&probe);
        assert_eq!(got, inner.canonicalize().unwrap());
    }

    #[test]
    fn root_for_path_unmatched_falls_back_to_default() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("repo_a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = RootRegistry::new(&a, vec![]);
        let other = std::env::temp_dir().join("definitely_not_under_repo_a");
        let got = reg.root_for_path(&other);
        assert_eq!(got, a.canonicalize().unwrap());
    }

    #[test]
    fn is_default_matches_canonicalized_default() {
        let tmp = TempDir::new().unwrap();
        let reg = RootRegistry::new(tmp.path(), vec![]);
        assert!(reg.is_default(tmp.path()));
    }

    // ── Named roots ────────────────────────────────────────────────────────

    #[test]
    fn resolve_name_finds_default_when_named() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = RootRegistry::with_named(&a, Some("primary".into()), Vec::new());
        let got = reg.resolve_name("primary").expect("default by name");
        assert_eq!(got, a.canonicalize().unwrap());
    }

    #[test]
    fn resolve_name_finds_additional_named_root() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let reg = RootRegistry::with_named(&a, None, vec![(Some("bee".into()), b.clone())]);
        let got = reg.resolve_name("bee").expect("by name");
        assert_eq!(got, b.canonicalize().unwrap());
    }

    #[test]
    fn resolve_name_returns_none_for_unknown_name() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let reg = RootRegistry::with_named(&a, None, Vec::new());
        assert!(reg.resolve_name("nope").is_none());
    }

    #[test]
    fn all_with_names_reports_default_name_when_present() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let reg = RootRegistry::with_named(
            &a,
            Some("primary".into()),
            vec![(Some("bee".into()), b.clone())],
        );
        let all = reg.all_with_names();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].0, Some("primary"));
        assert!(all[0].2);
        assert_eq!(all[1].0, Some("bee"));
        assert!(!all[1].2);
    }

    // ── ConfigFile ─────────────────────────────────────────────────────────

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("mcp.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn config_loads_valid_toml() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
default = "fff"

[[roots]]
name = "fff"
path = "/tmp/fff"

[[roots]]
name = "vec"
path = "/tmp/vec"

[[roots]]
path = "/tmp/anon"
"#,
        );
        let cfg = ConfigFile::load(&path).expect("parse ok");
        assert_eq!(cfg.roots.len(), 3);
        assert_eq!(cfg.default.as_deref(), Some("fff"));
        assert_eq!(cfg.resolve_default_path(), Some(PathBuf::from("/tmp/fff")));
    }

    #[test]
    fn config_default_as_absolute_path_resolves() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
default = "/tmp/anon"

[[roots]]
path = "/tmp/anon"
"#,
        );
        let cfg = ConfigFile::load(&path).expect("parse ok");
        assert_eq!(cfg.resolve_default_path(), Some(PathBuf::from("/tmp/anon")));
    }

    #[test]
    fn config_unknown_top_level_key_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(tmp.path(), "wat = 1\n");
        let err = ConfigFile::load(&path).unwrap_err();
        matches!(err, ConfigError::Parse { .. });
    }

    #[test]
    fn config_unknown_root_key_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[[roots]]
path = "/tmp/x"
unknown = "no"
"#,
        );
        let err = ConfigFile::load(&path).unwrap_err();
        matches!(err, ConfigError::Parse { .. });
    }

    #[test]
    fn config_default_unknown_name_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
default = "ghost"

[[roots]]
name = "real"
path = "/tmp/real"
"#,
        );
        let err = ConfigFile::load(&path).unwrap_err();
        match err {
            ConfigError::UnresolvedDefault { value, .. } => assert_eq!(value, "ghost"),
            other => panic!("expected UnresolvedDefault, got {other:?}"),
        }
    }

    #[test]
    fn config_duplicate_names_rejected() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[[roots]]
name = "a"
path = "/tmp/a"

[[roots]]
name = "a"
path = "/tmp/b"
"#,
        );
        let err = ConfigFile::load(&path).unwrap_err();
        match err {
            ConfigError::DuplicateName { name, .. } => assert_eq!(name, "a"),
            other => panic!("expected DuplicateName, got {other:?}"),
        }
    }

    #[test]
    fn config_parse_error_mentions_path() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(tmp.path(), "this is not toml = = =\n");
        let err = ConfigFile::load(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(path.to_str().unwrap()),
            "error message should mention the config path; got: {msg}"
        );
    }

    #[test]
    fn config_name_for_path_lookup() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            r#"
[[roots]]
name = "alpha"
path = "/tmp/alpha"

[[roots]]
path = "/tmp/beta"
"#,
        );
        let cfg = ConfigFile::load(&path).unwrap();
        assert_eq!(
            cfg.name_for_path(Path::new("/tmp/alpha")),
            Some("alpha".to_string())
        );
        assert_eq!(cfg.name_for_path(Path::new("/tmp/beta")), None);
    }
}
