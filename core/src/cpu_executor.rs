//! One reusable CPU-only Rayon pool, with nonblocking work admission.
//! Small or saturated queries stay on their requesting thread.
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};

const MAX_CPU_THREADS: usize = 8;

fn thread_count(available: usize) -> usize {
    // Preserve headroom for AppKit and indexing; cap where the measured gains
    // justify the workers' stacks and scheduling cost.
    available.div_ceil(2).clamp(1, MAX_CPU_THREADS)
}

struct WorkerBudget {
    available: AtomicUsize,
}

impl WorkerBudget {
    fn new(capacity: usize) -> Self {
        Self {
            available: AtomicUsize::new(capacity),
        }
    }

    fn try_acquire_exact(&self, count: usize) -> Option<WorkerPermit<'_>> {
        if count == 0 {
            return None;
        }
        self.available
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |available| {
                available.checked_sub(count)
            })
            .ok()?;
        Some(WorkerPermit {
            budget: self,
            count,
        })
    }

    fn try_acquire(&self, maximum: usize) -> Option<WorkerPermit<'_>> {
        let previous = self
            .available
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |available| {
                let count = maximum.min(available);
                (count > 0).then_some(available - count)
            })
            .ok()?;
        Some(WorkerPermit {
            budget: self,
            count: maximum.min(previous),
        })
    }
}

struct WorkerPermit<'a> {
    budget: &'a WorkerBudget,
    pub(crate) count: usize,
}

impl Drop for WorkerPermit<'_> {
    fn drop(&mut self) {
        self.budget
            .available
            .fetch_add(self.count, Ordering::Relaxed);
    }
}

struct CpuExecutor {
    pool: rayon::ThreadPool,
    budget: WorkerBudget,
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
            Some(CpuExecutor {
                pool,
                budget: WorkerBudget::new(workers),
            })
        })
        .as_ref()
}

pub(crate) fn acquire(maximum: usize) -> Option<CpuPermit> {
    let executor = executor()?;
    let allocation = executor.budget.try_acquire(maximum)?;
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
    let allocation = executor
        .budget
        .try_acquire_exact(executor.pool.current_num_threads())?;
    Some(CpuPermit {
        allocation,
        pool: &executor.pool,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
