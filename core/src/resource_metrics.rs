//! Process-local aggregate diagnostics. Never retain paths or query text.
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
pub(crate) static METADATA_READS: AtomicU64 = AtomicU64::new(0);
pub(crate) static MOUNT_REFRESHES: AtomicU64 = AtomicU64::new(0);
pub(crate) static MOUNT_CHECKS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PARENT_OPENS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PREPARATION_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static COMMIT_NS: AtomicU64 = AtomicU64::new(0);
pub(crate) static PREPARATION_RETRIES: AtomicU64 = AtomicU64::new(0);
pub(crate) struct Timer {
    start: Instant,
    counter: &'static AtomicU64,
}
impl Timer {
    pub(crate) fn new(counter: &'static AtomicU64) -> Self {
        Self {
            start: Instant::now(),
            counter,
        }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        self.counter.fetch_add(
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}
pub(crate) fn snapshot() -> Value {
    let (cache_bytes, cache_limit) = crate::derived_cache::usage();
    let (scratch_bytes, scratch_peak) = crate::query_scratch::usage();
    json!({"cache_estimated_bytes": cache_bytes, "cache_budget_bytes": cache_limit, "metadata_reads": METADATA_READS.load(Ordering::Relaxed),
        "active_subject_buffer_bytes": scratch_bytes, "peak_subject_buffer_bytes": scratch_peak,
        "mount_refreshes": MOUNT_REFRESHES.load(Ordering::Relaxed),
        "mount_checks": MOUNT_CHECKS.load(Ordering::Relaxed),
        "parent_opens": PARENT_OPENS.load(Ordering::Relaxed),
        "preparation_ms": PREPARATION_NS.load(Ordering::Relaxed) as f64 / 1e6,
        "commit_ms": COMMIT_NS.load(Ordering::Relaxed) as f64 / 1e6,
        "preparation_retries": PREPARATION_RETRIES.load(Ordering::Relaxed)})
}
