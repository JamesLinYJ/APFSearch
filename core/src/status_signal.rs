//! In-memory change notifications. Waiting does not poll SQLite or the filesystem.
use serde_json::Value;
use std::{
    ops::{Deref, DerefMut},
    sync::{
        Condvar, LockResult, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct VersionedStatus {
    value: Value,
    revision: u64,
}

pub(crate) struct StatusSignal {
    state: Mutex<VersionedStatus>,
    changed: Condvar,
    waiters: AtomicUsize,
}

impl StatusSignal {
    pub(crate) fn new(value: Value) -> Self {
        // A reconnect should not confuse the old service's revision with the new
        // service's first revision. This is an opaque change token, not a date.
        let revision = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self {
            state: Mutex::new(VersionedStatus { value, revision }),
            changed: Condvar::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    pub(crate) fn lock(&self) -> LockResult<StatusGuard<'_>> {
        match self.state.lock() {
            Ok(state) => Ok(StatusGuard {
                owner: self,
                state,
                dirty: false,
            }),
            Err(error) => Err(PoisonError::new(StatusGuard {
                owner: self,
                state: error.into_inner(),
                dirty: false,
            })),
        }
    }

    pub(crate) fn snapshot(&self) -> (Value, u64) {
        let state = self.state.lock().unwrap();
        (state.value.clone(), state.revision)
    }

    pub(crate) fn notify(&self) {
        let mut state = self.state.lock().unwrap();
        state.revision = state.revision.wrapping_add(1);
        self.changed.notify_all();
    }

    pub(crate) fn wake_cancelled(&self) {
        // Take the same mutex as the condition check to prevent a lost wakeup
        // between checking cancellation and going to sleep.
        let _state = self.state.lock().unwrap();
        self.changed.notify_all();
    }

    pub(crate) fn wait(
        &self,
        after: u64,
        timeout: Duration,
        cancelled: &AtomicBool,
    ) -> Result<(), String> {
        const MAX_WAITERS: usize = 32;
        if self.waiters.fetch_add(1, Ordering::AcqRel) >= MAX_WAITERS {
            self.waiters.fetch_sub(1, Ordering::AcqRel);
            return Err("Too many status observers; retry later".into());
        }
        struct Waiting<'a>(&'a AtomicUsize);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _waiting = Waiting(&self.waiters);
        let state = self.state.lock().map_err(|_| "Status lock poisoned")?;
        let _wait = self
            .changed
            .wait_timeout_while(state, timeout.min(Duration::from_secs(30)), |state| {
                state.revision == after && !cancelled.load(Ordering::Acquire)
            })
            .map_err(|_| "Status lock poisoned")?;
        if cancelled.load(Ordering::Acquire) {
            Err("Status observation cancelled".into())
        } else {
            Ok(())
        }
    }
}

pub(crate) struct StatusGuard<'a> {
    owner: &'a StatusSignal,
    state: MutexGuard<'a, VersionedStatus>,
    dirty: bool,
}
impl Deref for StatusGuard<'_> {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.state.value
    }
}
impl DerefMut for StatusGuard<'_> {
    fn deref_mut(&mut self) -> &mut Value {
        self.dirty = true;
        &mut self.state.value
    }
}
impl Drop for StatusGuard<'_> {
    fn drop(&mut self) {
        if self.dirty {
            self.state.revision = self.state.revision.wrapping_add(1);
            self.owner.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn reads_do_not_generate_more_notifications() {
        let signal = StatusSignal::new(json!({"state":"idle"}));
        let revision = signal.snapshot().1;
        assert_eq!(signal.lock().unwrap()["state"], "idle");
        assert_eq!(signal.snapshot().1, revision);
        signal.lock().unwrap()["state"] = json!("updating");
        assert_ne!(signal.snapshot().1, revision);
    }

    #[test]
    fn missed_changes_and_timeouts_return_without_mutating_status() {
        let signal = StatusSignal::new(json!({}));
        let revision = signal.snapshot().1;
        signal.notify();
        signal
            .wait(revision, Duration::from_secs(30), &AtomicBool::new(false))
            .unwrap();
        let changed = signal.snapshot().1;
        signal
            .wait(changed, Duration::ZERO, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(signal.snapshot().1, changed);
    }

    #[test]
    fn cancellation_wakes_a_waiter_and_releases_its_slot() {
        let signal = Arc::new(StatusSignal::new(json!({})));
        let cancelled = Arc::new(AtomicBool::new(false));
        let revision = signal.snapshot().1;
        let worker = {
            let signal = signal.clone();
            let cancelled = cancelled.clone();
            std::thread::spawn(move || signal.wait(revision, Duration::from_secs(30), &cancelled))
        };
        // Cancellation may win before registration; either ordering is correct.
        cancelled.store(true, Ordering::Release);
        signal.wake_cancelled();
        assert!(worker.join().unwrap().is_err());
        assert_eq!(signal.waiters.load(Ordering::Acquire), 0);
    }
}
