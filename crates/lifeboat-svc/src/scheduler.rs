//! Backup scheduler: an hourly timer plus a file watcher that runs a cycle 10 minutes after the
//! last change in a backed-up folder.

use crate::server::State;
use notify::{RecursiveMode, Watcher};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::{interval, sleep_until, Instant};

pub const HOURLY: Duration = Duration::from_secs(3600);
pub const DEBOUNCE: Duration = Duration::from_secs(600);

/// Run until `stop` becomes true.
pub async fn run(st: Arc<State>, mut stop: watch::Receiver<bool>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        if ev.is_ok() {
            let _ = tx.send(());
        }
    })
    .ok();
    if let Some(w) = watcher.as_mut() {
        let folders = st
            .cfg
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .folders
            .clone();
        for f in folders {
            let _ = w.watch(&f, RecursiveMode::Recursive);
        }
    }
    let mut tick = interval(HOURLY);
    tick.tick().await; // the first tick fires at once; the first cycle runs after an hour
    let mut due: Option<Instant> = None;
    loop {
        let wait = async {
            match due {
                Some(t) => sleep_until(t).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = stop.changed() => return,
            _ = tick.tick() => cycle(&st).await,
            Some(()) = rx.recv() => due = Some(Instant::now() + DEBOUNCE),
            _ = wait => { due = None; cycle(&st).await; }
        }
    }
}

async fn cycle(st: &Arc<State>) {
    let st = st.clone();
    let _ = tokio::task::spawn_blocking(move || st.run_backup()).await;
}
