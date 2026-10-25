//! Shared performance state: the volume lease, the restic result caches, the repository lock
//! and the disk-info cache. One `Arc<Ctx>` is shared by the request handlers and backup cycles.

use crate::cache::Caches;
use crate::lease::{Lease, Volume, IDLE};
use lifeboat_core::disk::DiskInfo;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

pub struct Ctx {
    pub lease: Lease,
    pub caches: Caches,
    /// Reads and backups share it; forget / prune / delete / check take it exclusively.
    pub repo_lock: RwLock<()>,
    /// partition guid (lowercase) -> disk holding it.
    disks: Mutex<HashMap<String, DiskInfo>>,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx::new()
    }
}

impl Ctx {
    pub fn new() -> Ctx {
        Ctx {
            lease: Lease::new(IDLE),
            caches: Caches::default(),
            repo_lock: RwLock::new(()),
            disks: Mutex::new(HashMap::new()),
        }
    }

    pub fn read(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        self.repo_lock.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.repo_lock.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The backup volume, located (disk online, partition unhidden) at most once per lease.
    /// An empty `guid` is the test override: no partition, nothing to lease.
    pub fn volume(&self, guid: &str) -> Result<Arc<Volume>, String> {
        self.lease.acquire(|| self.open(guid))
    }

    fn open(&self, guid: &str) -> Result<Volume, String> {
        let key = guid.to_lowercase();
        let cached = self
            .disks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        // A cached disk that no longer fits (renumbered, replugged) is looked up once more.
        if let Some(d) = cached {
            if let Ok(v) = open_on(guid, &d) {
                return Ok(v);
            }
        }
        let disk = lookup(guid)?;
        self.disks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, disk.clone());
        open_on(guid, &disk)
    }
}

fn lookup(guid: &str) -> Result<DiskInfo, String> {
    lifeboat_disk::list_disks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|d| {
            d.partitions
                .iter()
                .any(|p| lifeboat_core::disk::same_guid(&p.guid, guid))
        })
        .ok_or_else(|| "backup disk not found".to_string())
}

/// One PowerShell run: online (own disk only) + find + unhide. The closer is the matching
/// single run: hide + offline.
fn open_on(guid: &str, disk: &DiskInfo) -> Result<Volume, String> {
    let own = (!disk.is_shared()).then_some(disk);
    let path = lifeboat_disk::open_volume(guid, own)
        .map_err(|e| e.to_string())?
        .ok_or("backup volume not found or marker mismatch")?;
    let (g, d) = (guid.to_string(), own.cloned());
    Ok(Volume::new(path, move || {
        let _ = lifeboat_disk::close_volume(&g, d.as_ref());
    }))
}
