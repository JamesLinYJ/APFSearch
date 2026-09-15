//! Shared admission for requesting threads and the CPU-only Rayon pool.
//! Saturated requests wait for completion or cancellation, never a timer.
use std::sync::{
    Condvar, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const MAX_CPU_THREADS: usize = 8;
pub(crate) fn worker_capacity() -> usize {
    static CAPACITY: OnceLock<usize> = OnceLock::new();
    *CAPACITY.get_or_init(|| {
        thread_count(
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        )
    })
}

fn thread_count(available: usize) -> usize {
    // Preserve headroom for AppKit and indexing; cap where the measured gains
    // justify the workers' stacks and scheduling cost.
    available.div_ceil(2).clamp(1, MAX_CPU_THREADS)
}

struct WorkerBudget {
    available: AtomicUsize,
    foreground_waiters: AtomicUsize,
    waiters: AtomicUsize,
    background_active: AtomicUsize,
    wake_lock: Mutex<()>,
    completed: Condvar,
}

impl WorkerBudget {
    fn new(capacity: usize) -> Self {
        Self {
            available: AtomicUsize::new(capacity),
            foreground_waiters: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            background_active: AtomicUsize::new(0),
            wake_lock: Mutex::new(()),
            completed: Condvar::new(),
        }
    }

    fn try_acquire_exact(&self, count: usize) -> Option<WorkerPermit<'_>> {
        if count == 0 {
            return None;
        }
        self.available
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |available| {
                available.checked_sub(count)
            })
            .ok()?;
        Some(WorkerPermit {
            budget: self,
            count,
            background: false,
        })
    }

    fn try_acquire(&self, maximum: usize) -> Option<WorkerPermit<'_>> {
        let previous = self
            .available
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |available| {
                let count = maximum.min(available);
                (count > 0).then_some(available - count)
            })
            .ok()?;
        Some(WorkerPermit {
            budget: self,
            count: maximum.min(previous),
            background: false,
        })
    }
    fn enter(&self, cancelled: &AtomicBool) -> Result<WorkerPermit<'_>, String> {
        if cancelled.load(Ordering::Acquire) {
            return Err("Query cancelled".into());
        }
        // An uncontended admission only needs the atomic capacity reservation.
        // The condition-variable mutex belongs to the saturated waiting path.
        if let Some(permit) = self.try_acquire_exact(1) {
            return Ok(permit);
        }
        let mut wake = self.wake_lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::SeqCst);
        self.foreground_waiters.fetch_add(1, Ordering::Relaxed);
        loop {
            if cancelled.load(Ordering::Acquire) {
                self.foreground_waiters.fetch_sub(1, Ordering::Relaxed);
                self.waiters.fetch_sub(1, Ordering::SeqCst);
                self.completed.notify_all();
                return Err("Query cancelled".into());
            }
            if let Some(permit) = self.try_acquire_exact(1) {
                self.foreground_waiters.fetch_sub(1, Ordering::Relaxed);
                self.waiters.fetch_sub(1, Ordering::SeqCst);
                return Ok(permit);
            }
            wake = self.completed.wait(wake).unwrap();
        }
    }
    fn wake(&self) {
        let _wake = self.wake_lock.lock().unwrap();
        self.completed.notify_all();
    }
    fn enter_background(&self) -> WorkerPermit<'_> {
        let mut wake = self.wake_lock.lock().unwrap();
        self.waiters.fetch_add(1, Ordering::SeqCst);
        loop {
            let foreground = self.foreground_waiters.load(Ordering::Relaxed);
            if (foreground == 0 || self.background_active.load(Ordering::Relaxed) == 0)
                && (foreground == 0 || self.available.load(Ordering::Relaxed) > 1)
                && let Some(mut permit) = self.try_acquire_exact(1)
            {
                self.background_active.fetch_add(1, Ordering::Relaxed);
                self.waiters.fetch_sub(1, Ordering::SeqCst);
                permit.background = true;
                return permit;
            }
            wake = self.completed.wait(wake).unwrap();
        }
    }
}

struct WorkerPermit<'a> {
    budget: &'a WorkerBudget,
    pub(crate) count: usize,
    background: bool,
}

impl Drop for WorkerPermit<'_> {
    fn drop(&mut self) {
        if self.background {
            self.budget
                .background_active
                .fetch_sub(1, Ordering::Relaxed);
        }
        self.budget
            .available
            .fetch_add(self.count, Ordering::SeqCst);
        // Sequential ordering pairs release with waiter registration and its
        // subsequent capacity check: either it sees this capacity or we see
        // the waiter. The uncontended path needs no mutex or notification.
        if self.budget.waiters.load(Ordering::SeqCst) != 0 {
            let _wake = self.budget.wake_lock.lock().unwrap();
            self.budget.completed.notify_all();
        }
    }
}

struct CpuExecutor {
    pool: rayon::ThreadPool,
}
fn budget() -> &'static WorkerBudget {
    static BUDGET: OnceLock<WorkerBudget> = OnceLock::new();
    BUDGET.get_or_init(|| WorkerBudget::new(worker_capacity()))
}
pub(crate) struct QueryPermit {
    _permit: WorkerPermit<'static>,
}
pub(crate) fn enter_query(cancelled: &AtomicBool) -> Result<QueryPermit, String> {
    budget()
        .enter(cancelled)
        .map(|permit| QueryPermit { _permit: permit })
}
pub(crate) fn wake_cancelled() {
    budget().wake();
}
pub(crate) struct BackgroundPermit {
    permit: Option<WorkerPermit<'static>>,
}
pub(crate) fn enter_background() -> BackgroundPermit {
    BackgroundPermit {
        permit: Some(budget().enter_background()),
    }
}
impl BackgroundPermit {
    pub(crate) fn checkpoint(&mut self) {
        if budget().foreground_waiters.load(Ordering::Relaxed) > 0 {
            self.permit.take();
            self.permit = Some(budget().enter_background());
        }
    }
}

pub(crate) struct CpuPermit {
    allocation: WorkerPermit<'static>,
    pool: &'static rayon::ThreadPool,
}

impl CpuPermit {
    pub(crate) fn workers(&self) -> usize {
        self.allocation.count
    }

    pub(crate) fn run<R: Send>(&self, work: impl FnOnce() -> R + Send) -> R {
        self.pool.install(work)
    }
}

fn executor() -> Option<&'static CpuExecutor> {
    static EXECUTOR: OnceLock<Option<CpuExecutor>> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| {
            let available = std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1);
            let workers = thread_count(available);
            if workers <= 1 {
                return None;
            }
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(workers)
                .thread_name(|index| format!("apfsearch-cpu-{index}"))
                .build()
                .ok()?;
            Some(CpuExecutor { pool })
        })
        .as_ref()
}

pub(crate) fn acquire(maximum: usize) -> Option<CpuPermit> {
    let executor = executor()?;
    if budget().foreground_waiters.load(Ordering::Relaxed) > 0 {
        return None;
    }
    let allocation = budget().try_acquire(maximum)?;
    if allocation.count <= 1 {
        return None;
    }
    Some(CpuPermit {
        allocation,
        pool: &executor.pool,
    })
}

/// Library algorithms using recursive Rayon joins may occupy the whole pool.
/// Admit them only when the entire worker budget is available; never wait for it.
pub(crate) fn acquire_all() -> Option<CpuPermit> {
    let executor = executor()?;
    if budget().foreground_waiters.load(Ordering::Relaxed) > 0 {
        return None;
    }
    let allocation = budget().try_acquire_exact(executor.pool.current_num_threads())?;
    Some(CpuPermit {
        allocation,
        pool: &executor.pool,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_racing_waiter_registration_cannot_lose_a_wakeup() {
        for background in [false, true] {
            for _ in 0..64 {
                let budget = std::sync::Arc::new(WorkerBudget::new(1));
                let active = budget.try_acquire_exact(1).unwrap();
                let worker = budget.clone();
                let (started, ready) = std::sync::mpsc::channel();
                let (finished, done) = std::sync::mpsc::channel();
                let thread = std::thread::spawn(move || {
                    started.send(()).unwrap();
                    let permit = if background {
                        worker.enter_background()
                    } else {
                        worker.enter(&AtomicBool::new(false)).unwrap()
                    };
                    drop(permit);
                    finished.send(()).unwrap();
                });
                ready.recv().unwrap();
                drop(active);
                done.recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                thread.join().unwrap();
                assert_eq!(budget.available.load(Ordering::SeqCst), 1);
                assert_eq!(budget.waiters.load(Ordering::SeqCst), 0);
            }
        }
    }
    #[test]
    fn cancellation_wakes_saturated_admission_without_releasing_active_work() {
        let budget = WorkerBudget::new(1);
        let active = budget.try_acquire_exact(1).unwrap();
        let cancelled = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (budget, cancelled) = (&budget, &cancelled);
            let (send, receive) = std::sync::mpsc::channel();
            scope.spawn(move || {
                send.send(budget.enter(cancelled).is_err()).unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while budget.foreground_waiters.load(Ordering::Relaxed) == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
            cancelled.store(true, Ordering::Release);
            budget.wake();
            assert!(
                receive
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap()
            );
            assert_eq!(budget.available.load(Ordering::Relaxed), 0);
        });
        drop(active);
        assert_eq!(budget.available.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn pool_capacity_scales_with_hardware_and_keeps_headroom() {
        for (available, expected) in [(0, 1), (1, 1), (2, 1), (4, 2), (8, 4), (15, 8), (64, 8)] {
            assert_eq!(thread_count(available), expected);
        }
    }
    #[test]
    fn overlapping_queries_share_capacity_without_waiting_and_restore_it() {
        let budget = WorkerBudget::new(3);
        let first = budget.try_acquire(2).unwrap();
        let second = std::thread::scope(|scope| {
            scope
                .spawn(|| budget.try_acquire(3).unwrap())
                .join()
                .unwrap()
        });
        assert_eq!(first.count, 2);
        assert_eq!(second.count, 1);
        assert!(budget.try_acquire(1).is_none());
        drop(first);
        assert_eq!(budget.available.load(Ordering::Relaxed), 2);
        drop(second);
        assert_eq!(budget.try_acquire(8).unwrap().count, 3);
        assert!(budget.try_acquire(0).is_none());
    }
    #[test]
    fn whole_pool_work_does_not_take_capacity_from_active_queries() {
        let budget = WorkerBudget::new(4);
        let query = budget.try_acquire(2).unwrap();
        assert!(budget.try_acquire_exact(4).is_none());
        assert_eq!(budget.available.load(Ordering::Relaxed), 2);
        drop(query);
        let whole_pool = budget.try_acquire_exact(4).unwrap();
        assert!(budget.try_acquire(1).is_none());
        drop(whole_pool);
        assert_eq!(budget.available.load(Ordering::Relaxed), 4);
    }
    #[test]
    fn racing_requests_cannot_exceed_the_process_budget() {
        let budget = WorkerBudget::new(3);
        let barrier = std::sync::Barrier::new(9);
        let acquired = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    let permit = budget.try_acquire(1);
                    if permit.is_some() {
                        acquired.fetch_add(1, Ordering::Relaxed);
                    }
                    barrier.wait();
                    barrier.wait();
                    drop(permit);
                });
            }
            barrier.wait();
            barrier.wait();
            let granted = acquired.load(Ordering::Relaxed);
            let remaining = budget.available.load(Ordering::Relaxed);
            barrier.wait();
            assert_eq!(granted, 3);
            assert_eq!(remaining, 0);
        });
        assert_eq!(budget.available.load(Ordering::Relaxed), 3);
    }
}
