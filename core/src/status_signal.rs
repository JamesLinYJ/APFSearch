//! Revisioned in-memory status notifications. No timer, database writes, or
//! filesystem reads are required while clients wait for an observable change.
use serde_json::Value;
use std::{
    ops::{Deref, DerefMut},
    sync::{Condvar, LockResult, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

pub(crate) struct StatusSignal {
    value: Mutex<Value>,
    revision: Mutex<u64>,
    changed: Condvar,
}

impl StatusSignal {
    pub(crate) fn new(value: Value) -> Self {
        Self {
            value: Mutex::new(value),
            revision: Mutex::new(1),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn lock(&self) -> LockResult<StatusGuard<'_>> {
        match self.value.lock() {
            Ok(value) => Ok(StatusGuard { owner: self, value: Some(value), dirty: false }),
            Err(error) => Err(PoisonError::new(StatusGuard {
                owner: self,
                value: Some(error.into_inner()),
                dirty: false,
            })),
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        *self.revision.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn notify(&self) {
        let mut revision = self.revision.lock().unwrap_or_else(PoisonError::into_inner);
        *revision = revision.wrapping_add(1);
        self.changed.notify_all();
    }

    /// Both the predicate and wait use the same mutex. A publication between
    /// the client's read and this call cannot be lost as a missed wakeup.
    pub(crate) fn wait_after(&self, after: u64, timeout: Duration) {
        let revision = self.revision.lock().unwrap_or_else(PoisonError::into_inner);
        drop(self.changed.wait_timeout_while(revision, timeout, |current| *current == after));
    }
}

pub(crate) struct StatusGuard<'a> {
    owner: &'a StatusSignal,
    value: Option<MutexGuard<'a, Value>>,
    dirty: bool,
}

impl Deref for StatusGuard<'_> {
    type Target = Value;
    fn deref(&self) -> &Value { self.value.as_deref().expect("live status guard") }
}

impl DerefMut for StatusGuard<'_> {
    fn deref_mut(&mut self) -> &mut Value {
        self.dirty = true;
        self.value.as_deref_mut().expect("live status guard")
    }
}

impl Drop for StatusGuard<'_> {
    fn drop(&mut self) {
        // Never acquire the notification mutex while holding the value lock.
        drop(self.value.take());
        if self.dirty { self.owner.notify(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{mpsc, Arc, Barrier};

    #[test]
    fn read_only_status_access_does_not_publish_a_change() {
        let state = StatusSignal::new(json!({"state":"idle"}));
        let before = state.revision();
        assert_eq!(state.lock().unwrap()["state"], "idle");
        assert_eq!(state.revision(), before);
        state.lock().unwrap()["state"] = json!("updating");
        assert_ne!(state.revision(), before);
    }

    #[test]
    fn all_waiters_wake_and_an_earlier_publication_is_not_lost() {
        let state = Arc::new(StatusSignal::new(json!({})));
        let before = state.revision();
        let barrier = Arc::new(Barrier::new(3));
        let (send, receive) = mpsc::channel();
        let workers: Vec<_> = (0..2).map(|_| {
            let state = state.clone();
            let barrier = barrier.clone();
            let send = send.clone();
            std::thread::spawn(move || {
                barrier.wait();
                state.wait_after(before, Duration::from_secs(5));
                send.send(state.revision()).unwrap();
            })
        }).collect();
        barrier.wait();
        state.notify();
        for _ in 0..2 {
            assert_ne!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), before);
        }
        for worker in workers { worker.join().unwrap(); }
        // The stale revision must return immediately even when the notification
        // happened before the wait was registered.
        state.wait_after(before, Duration::from_secs(5));
        assert_ne!(state.revision(), before);
    }
}
