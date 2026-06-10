//! Startup registry of project roots fff-mcp can search.
//!
//! `--base-path` sets the default; `--root` flags add additional roots.
//! All paths are canonicalized once at construction so downstream lookups
//! (pool, longest-prefix match) compare against absolute paths.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct RootRegistry {
    default: PathBuf,
    additional: Vec<PathBuf>,
}

impl RootRegistry {
    /// Build a registry from CLI args. `default` is the value of `--base-path`
    /// (or the discovered git root); `extras` are the `--root` values.
    /// All entries are canonicalized; duplicates against the default and
    /// across `extras` are silently dropped.
    pub fn new<P: AsRef<Path>>(default: P, extras: impl IntoIterator<Item = PathBuf>) -> Self {
        let default = canonicalize(default.as_ref());
        let mut additional: Vec<PathBuf> = Vec::new();
        for raw in extras {
            let canon = canonicalize(&raw);
            if canon == default {
                continue;
            }
            if additional.iter().any(|p| p == &canon) {
                continue;
            }
            additional.push(canon);
        }
        Self {
            default,
            additional,
        }
    }

    pub fn default_root(&self) -> &Path {
        &self.default
    }

    #[allow(dead_code)]
    pub fn additional(&self) -> &[PathBuf] {
        &self.additional
    }

    /// All registered roots, default first. Returns `(path, is_default)` pairs
    /// suitable for the `list_roots` MCP response.
    pub fn all(&self) -> Vec<(&Path, bool)> {
        let mut out: Vec<(&Path, bool)> = Vec::with_capacity(1 + self.additional.len());
        out.push((self.default.as_path(), true));
        for p in &self.additional {
            out.push((p.as_path(), false));
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
            .chain(self.additional.iter().map(|p| p.as_path()))
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
}
