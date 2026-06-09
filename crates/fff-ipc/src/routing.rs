use std::{collections::HashMap, fs, io, path::Path};

use serde::{Deserialize, Serialize};

/// Persistent routing state: ring snapshot + per-worker loaded roots.
/// Written atomically to routing.json; loaded by master on startup to
/// reconnect surviving workers after a crash.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RoutingTable {
    /// Serializable snapshot of the consistent hash ring (virtual nodes).
    pub ring_state: SerializableRing,
    /// worker_index → WorkerEntry for all currently registered workers.
    pub workers: HashMap<u32, WorkerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootEntry {
    pub slug: String,
    pub base_path: String,
}

/// Per-worker persistent state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEntry {
    pub index: u32,
    pub socket_path: String,
    pub pid: u32,
    #[serde(default)]
    pub roots: Vec<RootEntry>,
    /// Legacy field — read on load only, never written. Migrated into
    /// `roots` by `RoutingTable::load`. Private so new code paths can't
    /// touch it.
    #[serde(default, rename = "root_slugs", skip_serializing)]
    legacy_root_slugs: Vec<String>,
}

impl WorkerEntry {
    pub fn new(index: u32, socket_path: String, pid: u32) -> Self {
        Self {
            index,
            socket_path,
            pid,
            roots: Vec::new(),
            legacy_root_slugs: Vec::new(),
        }
    }

    pub fn contains_slug(&self, slug: &str) -> bool {
        self.roots.iter().any(|r| r.slug == slug)
    }

    pub fn push_root(&mut self, slug: String, base_path: String) {
        self.roots.push(RootEntry { slug, base_path });
    }

    /// Returns true if a matching slug was removed.
    pub fn remove_slug(&mut self, slug: &str) -> bool {
        let before = self.roots.len();
        self.roots.retain(|r| r.slug != slug);
        before != self.roots.len()
    }

    fn migrate_legacy(&mut self) {
        if self.roots.is_empty() && !self.legacy_root_slugs.is_empty() {
            self.roots = std::mem::take(&mut self.legacy_root_slugs)
                .into_iter()
                .map(|slug| RootEntry { slug, base_path: String::new() })
                .collect();
        } else {
            self.legacy_root_slugs.clear();
        }
    }
}

/// Serializable form of the hash ring's virtual-node list.
/// The actual `HashRing` type lives in `fff-engine` and imports this as its
/// persistence representation so that `fff-ipc` stays dependency-free from
/// engine internals.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SerializableRing {
    /// Sorted list of `(ring_point, worker_index)` virtual nodes.
    pub nodes: Vec<(u64, u32)>,
}

impl RoutingTable {
    /// Number of roots currently assigned to `worker_index`.
    pub fn entries_for_worker(&self, worker_index: u32) -> usize {
        self.workers
            .get(&worker_index)
            .map(|e| e.roots.len())
            .unwrap_or(0)
    }

    /// Total number of registered workers.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Load from a JSON file. Returns `Ok(Default)` when the file is absent.
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(s) => {
                let mut table: Self = serde_json::from_str(&s)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                for entry in table.workers.values_mut() {
                    entry.migrate_legacy();
                }
                Ok(table)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Atomically persist to a JSON file (write-to-tmp then rename).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, json)?;
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_entry(index: u32, slugs: &[&str]) -> WorkerEntry {
        let mut e = WorkerEntry::new(
            index,
            format!("worker-{index}.sock"),
            1000 + index,
        );
        for s in slugs {
            e.push_root((*s).to_string(), format!("/test/{s}"));
        }
        e
    }

    #[test]
    fn routing_table_round_trips_json() {
        let mut rt = RoutingTable::default();
        rt.workers.insert(0, make_entry(0, &["abc", "def"]));
        rt.workers.insert(1, make_entry(1, &[]));
        rt.ring_state = SerializableRing {
            nodes: vec![(100, 0), (500, 1)],
        };

        let json = serde_json::to_string(&rt).unwrap();
        let rt2: RoutingTable = serde_json::from_str(&json).unwrap();

        assert_eq!(rt2.workers.len(), 2);
        assert_eq!(
            rt2.workers[&0]
                .roots
                .iter()
                .map(|r| r.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["abc", "def"]
        );
        assert_eq!(rt2.ring_state.nodes, vec![(100, 0), (500, 1)]);
    }

    #[test]
    fn entries_for_worker_counts_slugs() {
        let mut rt = RoutingTable::default();
        rt.workers.insert(0, make_entry(0, &["a", "b", "c"]));
        rt.workers.insert(1, make_entry(1, &[]));

        assert_eq!(rt.entries_for_worker(0), 3);
        assert_eq!(rt.entries_for_worker(1), 0);
        assert_eq!(rt.entries_for_worker(99), 0);
    }

    #[test]
    fn load_returns_default_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("routing.json");
        let rt = RoutingTable::load(&path).unwrap();
        assert!(rt.workers.is_empty());
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("routing.json");

        let mut rt = RoutingTable::default();
        rt.workers.insert(0, make_entry(0, &["slug1"]));
        rt.save(&path).unwrap();

        let rt2 = RoutingTable::load(&path).unwrap();
        assert_eq!(
            rt2.workers[&0]
                .roots
                .iter()
                .map(|r| r.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["slug1"]
        );
    }

    #[test]
    fn root_entry_serializes_as_object() {
        let r = RootEntry {
            slug: "abc123".into(),
            base_path: "/home/me/proj".into(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"slug\":\"abc123\""));
        assert!(json.contains("\"base_path\":\"/home/me/proj\""));
    }

    #[test]
    fn worker_entry_round_trips_with_roots() {
        let mut rt = RoutingTable::default();
        let mut entry = WorkerEntry::new(0, "worker-0.sock".into(), 1000);
        entry.push_root("s1".into(), "/a".into());
        entry.push_root("s2".into(), "/b".into());
        rt.workers.insert(0, entry);

        let json = serde_json::to_string(&rt).unwrap();
        assert!(!json.contains("root_slugs"), "legacy key must not be written");

        let rt2: RoutingTable = serde_json::from_str(&json).unwrap();
        assert_eq!(rt2.workers[&0].roots[0].base_path, "/a");
        assert_eq!(rt2.workers[&0].roots[1].slug, "s2");
    }

    #[test]
    fn load_migrates_legacy_root_slugs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("routing.json");
        let legacy_json = r#"{
            "ring_state": { "nodes": [] },
            "workers": {
                "0": {
                    "index": 0,
                    "socket_path": "worker-0.sock",
                    "pid": 1000,
                    "root_slugs": ["legacyslug1", "legacyslug2"]
                }
            }
        }"#;
        std::fs::write(&path, legacy_json).unwrap();

        let rt = RoutingTable::load(&path).unwrap();
        let entry = &rt.workers[&0];
        assert_eq!(entry.roots.len(), 2);
        assert_eq!(entry.roots[0].slug, "legacyslug1");
        assert_eq!(entry.roots[0].base_path, "", "legacy entries hydrate with empty base_path");
        assert_eq!(entry.roots[1].slug, "legacyslug2");
    }

    #[test]
    fn save_is_atomic_no_partial_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("subdir").join("routing.json");
        let mut rt = RoutingTable::default();
        rt.workers.insert(0, make_entry(0, &["x"]));
        // Parent dir doesn't exist yet — save should create it.
        rt.save(&path).unwrap();
        assert!(path.exists());
        // No .tmp residue.
        assert!(!path.with_extension("json.tmp").exists());
    }
}
