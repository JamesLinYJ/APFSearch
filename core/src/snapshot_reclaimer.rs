//! Bounded, event-driven retirement of large snapshot payloads. Releasing a
//! reader drops its lease immediately; only unreachable storage is queued.
use crate::index_store::SnapshotData;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
};

const QUEUED_SNAPSHOTS: usize = 2;
enum Work {
    Snapshot(Arc<SnapshotData>),
    Barrier(mpsc::SyncSender<()>),
}
struct Reclaimer {
    sender: mpsc::SyncSender<Work>,
    pending: Arc<Mutex<HashMap<usize, Weak<SnapshotData>>>>,
}
static RECLAIMER: OnceLock<Reclaimer> = OnceLock::new();
fn reclaimer() -> &'static Reclaimer {
    RECLAIMER.get_or_init(|| {
        let (sender, receiver) = mpsc::sync_channel::<Work>(QUEUED_SNAPSHOTS);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let completed = pending.clone();
        std::thread::Builder::new()
            .name("apfsearch-reclaim".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    let data = match work {
                        Work::Snapshot(data) => data,
                        Work::Barrier(completed) => {
                            let _ = completed.send(());
                            continue;
                        }
                    };
                    let identity = Arc::as_ptr(&data) as usize;
                    let mut cpu = crate::cpu_executor::enter_background();
                    if let Ok(data) = Arc::try_unwrap(data) {
                        data.reclaim(&mut cpu);
                    }
                    completed.lock().unwrap().remove(&identity);
                }
            })
            .expect("Unable to start snapshot reclamation worker");
        Reclaimer { sender, pending }
    })
}
pub(crate) fn retire(data: Arc<SnapshotData>) {
    // Small fixtures and one-block snapshots have bounded destruction work and
    // do not need a separate worker. There is no elapsed-time/idle heuristic.
    if data.entries.len() <= crate::entry_table::CHUNK_LENGTH {
        return;
    }
    let reclaimer = reclaimer();
    let identity = Arc::as_ptr(&data) as usize;
    reclaimer
        .pending
        .lock()
        .unwrap()
        .insert(identity, Arc::downgrade(&data));
    // Backpressure bounds retired storage. Callers must release work permits
    // before their final snapshot reference, so a full queue cannot deadlock
    // the background worker waiting for the same CPU budget.
    if let Err(error) = reclaimer.sender.send(Work::Snapshot(data)) {
        reclaimer.pending.lock().unwrap().remove(&identity);
        drop(error.0);
    }
}
pub(crate) fn pending() -> Vec<Arc<SnapshotData>> {
    let Some(reclaimer) = RECLAIMER.get() else {
        return Vec::new();
    };
    reclaimer
        .pending
        .lock()
        .unwrap()
        .values()
        .filter_map(Weak::upgrade)
        .collect()
}
/// Full rebuilds drain retired versions before allocating another complete
/// index. This waits for work completion, never for a fixed idle delay.
pub(crate) fn wait_until_drained() {
    if let Some(reclaimer) = RECLAIMER.get() {
        let (sender, receiver) = mpsc::sync_channel(0);
        if reclaimer.sender.send(Work::Barrier(sender)).is_ok() {
            let _ = receiver.recv();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_is_bounded_and_completion_releases_the_payload() {
        let data = Arc::new(SnapshotData::default());
        let observed = Arc::downgrade(&data);
        let reclaimer = reclaimer();
        reclaimer.sender.send(Work::Snapshot(data)).unwrap();
        wait_until_drained();
        assert!(observed.upgrade().is_none());
    }
}
