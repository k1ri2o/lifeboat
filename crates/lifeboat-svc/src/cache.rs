//! In-memory caches of restic results, so browsing does not start restic (and re-derive the
//! repository key) for every click.
//!
//! * snapshot list per repository, dropped after anything that changes the repository;
//! * file tree per snapshot id: snapshots are immutable, so one `restic ls` serves every folder
//!   of that snapshot. A small LRU bounds memory; a snapshot with too many nodes falls back to
//!   caching one folder level at a time.

use lifeboat_engine::{LsNode, Snapshot};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Snapshots whose full tree is kept in memory at once.
pub const TREE_LRU: usize = 3;
/// A snapshot with more nodes than this is not held as a full tree (bounded memory).
pub const MAX_TREE_NODES: usize = 200_000;

type SizeKey = (String, String, String);
type Dirs = HashMap<String, Vec<LsNode>>;

/// One snapshot's tree. `full` is filled once; when it is `None` (too big) `partial` holds the
/// folder levels listed so far.
#[derive(Default)]
pub struct Tree {
    full: OnceLock<Option<Dirs>>,
    partial: Mutex<Dirs>,
}

impl Tree {
    /// Children of `dir` (restic path). `load_all` returns the whole tree or `None` when too
    /// big or failed; `load_dir` lists one level.
    pub fn dir(
        &self,
        dir: &str,
        load_all: impl FnOnce() -> Option<Vec<LsNode>>,
        load_dir: impl FnOnce() -> Result<Vec<LsNode>, String>,
    ) -> Result<Vec<LsNode>, String> {
        let dir = dir.trim_end_matches('/');
        let full = self
            .full
            .get_or_init(|| load_all().map(lifeboat_engine::group_by_dir));
        if let Some(map) = full {
            return Ok(map.get(dir).cloned().unwrap_or_default());
        }
        if let Some(v) = self
            .partial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(dir)
        {
            return Ok(v.clone());
        }
        let v = load_dir()?;
        self.partial
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(dir.to_string(), v.clone());
        Ok(v)
    }

    #[cfg(test)]
    pub fn is_full(&self) -> bool {
        matches!(self.full.get(), Some(Some(_)))
    }
}

#[derive(Default)]
pub struct Caches {
    generation: AtomicU64,
    snaps: Mutex<HashMap<String, Vec<Snapshot>>>,
    trees: Mutex<VecDeque<(String, Arc<Tree>)>>,
    /// (repo, snapshot id, path) -> (is a folder, total size of files below); immutable.
    sizes: Mutex<HashMap<SizeKey, (bool, u64)>>,
}

impl Caches {
    /// Snapshot list of `repo`; `load` runs restic on a miss. A result loaded while the
    /// repository changed meanwhile is returned but not stored.
    pub fn snapshots(
        &self,
        repo: &str,
        load: impl FnOnce() -> Result<Vec<Snapshot>, String>,
    ) -> Result<Vec<Snapshot>, String> {
        if let Some(v) = self
            .snaps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(repo)
        {
            return Ok(v.clone());
        }
        let gen = self.generation.load(Ordering::SeqCst);
        let v = load()?;
        let mut g = self.snaps.lock().unwrap_or_else(|e| e.into_inner());
        if self.generation.load(Ordering::SeqCst) == gen {
            g.insert(repo.to_string(), v.clone());
        }
        Ok(v)
    }

    /// The tree entry of one snapshot (created on first use, most recently used last).
    pub fn tree(&self, repo: &str, snapshot: &str) -> Arc<Tree> {
        let key = format!("{repo}\u{0}{snapshot}");
        let mut g = self.trees.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = g.iter().position(|(k, _)| *k == key) {
            let e = g.remove(i).expect("index in range");
            g.push_back(e);
        } else {
            g.push_back((key, Arc::new(Tree::default())));
            while g.len() > TREE_LRU {
                g.pop_front();
            }
        }
        g.back().expect("just pushed").1.clone()
    }

    /// Cached `(is folder, total file size)` of a path inside a snapshot.
    pub fn size_of(
        &self,
        repo: &str,
        snapshot: &str,
        path: &str,
        load: impl FnOnce() -> Result<(bool, u64), String>,
    ) -> Result<(bool, u64), String> {
        let key = (repo.to_string(), snapshot.to_string(), path.to_lowercase());
        if let Some(v) = self
            .sizes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Ok(*v);
        }
        let v = load()?;
        self.sizes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, v);
        Ok(v)
    }

    /// The repository changed (backup, vault add): the snapshot list is stale. Trees of
    /// existing snapshots stay valid.
    pub fn invalidate_snapshots(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.snaps.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Snapshots were removed (forget, prune, delete): drop everything.
    pub fn invalidate_all(&self) {
        self.invalidate_snapshots();
        self.trees.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.sizes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    #[cfg(test)]
    pub fn has_snapshots(&self, repo: &str) -> bool {
        self.snaps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn node(path: &str, dir: bool) -> LsNode {
        LsNode {
            name: lifeboat_engine::split_restic(path).1,
            path: path.into(),
            dir,
            size: 5,
            mtime: String::new(),
        }
    }

    fn snap(id: &str) -> Snapshot {
        Snapshot {
            id: id.into(),
            short_id: id[..2].into(),
            time: "2026-01-01T00:00:00Z".into(),
            paths: vec![],
            hostname: String::new(),
            tags: vec![],
        }
    }

    #[test]
    fn snapshot_list_cached_until_invalidated() {
        let c = Caches::default();
        let n = Cell::new(0);
        let load = || {
            n.set(n.get() + 1);
            Ok(vec![snap("aabbccdd")])
        };
        c.snapshots("r", load).unwrap();
        c.snapshots("r", load).unwrap();
        assert_eq!(n.get(), 1);
        c.invalidate_snapshots(); // after a backup
        c.snapshots("r", load).unwrap();
        assert_eq!(n.get(), 2);
        // Other repos are separate.
        c.snapshots("r2", load).unwrap();
        assert_eq!(n.get(), 3);
    }

    #[test]
    fn result_loaded_during_a_change_is_not_stored() {
        let c = Caches::default();
        c.snapshots("r", || {
            c.invalidate_snapshots(); // a backup finished while restic was listing
            Ok(vec![snap("aabbccdd")])
        })
        .unwrap();
        assert!(!c.has_snapshots("r"));
    }

    #[test]
    fn errors_are_not_cached() {
        let c = Caches::default();
        assert!(c.snapshots("r", || Err("boom".into())).is_err());
        assert!(!c.has_snapshots("r"));
    }

    #[test]
    fn full_tree_serves_every_folder_from_one_load() {
        let c = Caches::default();
        let loads = Cell::new(0);
        let all = || {
            loads.set(loads.get() + 1);
            Some(vec![
                node("/C", true),
                node("/C/a", true),
                node("/C/a/f.txt", false),
                node("/C/b.txt", false),
            ])
        };
        let none = || -> Result<Vec<LsNode>, String> { panic!("must not list a single level") };
        let t = c.tree("r", "id1");
        assert_eq!(t.dir("/C", all, none).unwrap().len(), 2);
        let t = c.tree("r", "id1");
        let sub = t.dir("/C/a/", all, none).unwrap();
        assert_eq!(sub[0].name, "f.txt");
        assert!(t.dir("/C/missing", all, none).unwrap().is_empty());
        assert_eq!(loads.get(), 1);
        assert!(t.is_full());
    }

    #[test]
    fn too_big_snapshot_falls_back_to_per_folder_cache() {
        let c = Caches::default();
        let lists = Cell::new(0);
        let list = || {
            lists.set(lists.get() + 1);
            Ok(vec![node("/C/x", false)])
        };
        let t = c.tree("r", "big");
        t.dir("/C", || None, list).unwrap();
        t.dir("/C", || panic!("full load is tried once"), list)
            .unwrap();
        assert_eq!(lists.get(), 1);
        assert!(!t.is_full());
    }

    #[test]
    fn lru_keeps_a_few_and_invalidate_all_drops_them() {
        let c = Caches::default();
        let first = c.tree("r", "s0");
        for i in 1..=TREE_LRU {
            c.tree("r", &format!("s{i}"));
        }
        // s0 was evicted: a new entry, not the old one.
        assert!(!Arc::ptr_eq(&first, &c.tree("r", "s0")));
        let kept = c.tree("r", "s0");
        assert!(Arc::ptr_eq(&kept, &c.tree("r", "s0")));
        c.invalidate_all(); // after forget / prune / delete
        assert!(!Arc::ptr_eq(&kept, &c.tree("r", "s0")));
    }

    #[test]
    fn snapshot_invalidation_keeps_trees() {
        let c = Caches::default();
        let t = c.tree("r", "s0");
        c.invalidate_snapshots();
        assert!(Arc::ptr_eq(&t, &c.tree("r", "s0")));
    }

    #[test]
    fn sizes_cached_per_snapshot_and_path() {
        let c = Caches::default();
        let n = Cell::new(0);
        let load = || {
            n.set(n.get() + 1);
            Ok((true, 42))
        };
        assert_eq!(c.size_of("r", "s", "/C/A", load).unwrap(), (true, 42));
        assert_eq!(c.size_of("r", "s", "/c/a", load).unwrap(), (true, 42));
        assert_eq!(n.get(), 1);
    }
}
