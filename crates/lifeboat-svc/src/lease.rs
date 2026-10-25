//! Volume lease: keeps the located (unhidden, online) backup volume for a short idle time so a
//! burst of requests costs one PowerShell round trip instead of one per request.
//!
//! The volume is reached by its `\\?\Volume{GUID}\` path (no drive letter), so while the lease
//! is held it is not visible in Explorer. When the lease is dropped the partition is hidden and
//! the disk taken offline again by the volume's closer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unused lease is kept.
pub const IDLE: Duration = Duration::from_secs(30);

type Closer = Box<dyn FnOnce() + Send + Sync>;

/// A located volume. Dropping it runs the closer (hide + offline).
pub struct Volume {
    pub path: String,
    closer: Option<Closer>,
}

impl Volume {
    pub fn new(path: String, closer: impl FnOnce() + Send + Sync + 'static) -> Volume {
        Volume {
            path,
            closer: Some(Box::new(closer)),
        }
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        if let Some(c) = self.closer.take() {
            c();
        }
    }
}

struct Slot {
    vol: Arc<Volume>,
    last_used: Instant,
}

/// Shared, idle-expiring holder of the located volume.
pub struct Lease {
    slot: Mutex<Option<Slot>>,
    idle: Duration,
}

impl Lease {
    pub fn new(idle: Duration) -> Lease {
        Lease {
            slot: Mutex::new(None),
            idle,
        }
    }

    /// Reuse the live volume, or locate it with `open`. Only one locate runs at a time.
    pub fn acquire(
        &self,
        open: impl FnOnce() -> Result<Volume, String>,
    ) -> Result<Arc<Volume>, String> {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = g.as_mut() {
            s.last_used = Instant::now();
            return Ok(s.vol.clone());
        }
        let vol = Arc::new(open()?);
        *g = Some(Slot {
            vol: vol.clone(),
            last_used: Instant::now(),
        });
        Ok(vol)
    }

    /// Drop the volume when it has been idle for the full idle time and nobody uses it.
    /// Returns true when it was dropped.
    pub fn expire_idle(&self) -> bool {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let expired = matches!(
            g.as_ref(),
            Some(s) if s.last_used.elapsed() >= self.idle && Arc::strong_count(&s.vol) == 1
        );
        if expired {
            *g = None;
        }
        expired
    }

    /// Drop the volume now (shutdown, or the disk layout changed).
    pub fn release_now(&self) {
        let old = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(old);
    }

    #[cfg(test)]
    pub fn is_held(&self) -> bool {
        self.slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counted() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
        (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)))
    }

    fn open(opens: &Arc<AtomicUsize>, closes: &Arc<AtomicUsize>) -> Result<Volume, String> {
        opens.fetch_add(1, Ordering::SeqCst);
        let c = closes.clone();
        Ok(Volume::new("V:/".into(), move || {
            c.fetch_add(1, Ordering::SeqCst);
        }))
    }

    #[test]
    fn reuses_live_volume() {
        let (o, c) = counted();
        let l = Lease::new(Duration::from_secs(30));
        let a = l.acquire(|| open(&o, &c)).unwrap();
        let b = l.acquire(|| open(&o, &c)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(o.load(Ordering::SeqCst), 1);
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn expires_only_when_idle_and_unused() {
        let (o, c) = counted();
        let l = Lease::new(Duration::from_millis(40));
        let a = l.acquire(|| open(&o, &c)).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        // Still in use: kept.
        assert!(!l.expire_idle());
        drop(a);
        assert!(l.expire_idle());
        assert_eq!(c.load(Ordering::SeqCst), 1, "closer ran: volume re-hidden");
        assert!(!l.is_held());
        // Next acquire opens again.
        l.acquire(|| open(&o, &c)).unwrap();
        assert_eq!(o.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn use_resets_idle_clock() {
        let (o, c) = counted();
        let l = Lease::new(Duration::from_millis(80));
        drop(l.acquire(|| open(&o, &c)).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        drop(l.acquire(|| open(&o, &c)).unwrap());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!l.expire_idle(), "touched 50 ms ago, idle is 80 ms");
        assert_eq!(o.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn release_now_hides_and_failed_open_holds_nothing() {
        let (o, c) = counted();
        let l = Lease::new(Duration::from_secs(30));
        assert!(l.acquire(|| Err("nope".into())).is_err());
        assert!(!l.is_held());
        let a = l.acquire(|| open(&o, &c)).unwrap();
        drop(a);
        l.release_now();
        assert_eq!(c.load(Ordering::SeqCst), 1);
        assert!(!l.is_held());
    }
}
