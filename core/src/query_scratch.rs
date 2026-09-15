//! Bounded reusable subject buffers. Retained capacity shares the derived-cache
//! budget; active capacity is recorded separately and never hidden in TLS.
use crate::derived_cache::{BufferReservation, reserve_buffer};
use std::{
    ops::Deref,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static EPOCH: AtomicUsize = AtomicUsize::new(0);
struct Retained<T> {
    value: T,
    _reservation: BufferReservation,
}
fn texts() -> &'static Mutex<Vec<Retained<String>>> {
    static POOL: OnceLock<Mutex<Vec<Retained<String>>>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
fn bytes() -> &'static Mutex<Vec<Retained<Vec<u8>>>> {
    static POOL: OnceLock<Mutex<Vec<Retained<Vec<u8>>>>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
fn add_active(bytes: usize) {
    if bytes == 0 {
        return;
    }
    let total = ACTIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(total, Ordering::Relaxed);
}
pub struct TextScratch {
    value: String,
    epoch: usize,
}
impl TextScratch {
    pub fn acquire(capacity: usize) -> Self {
        let epoch = EPOCH.load(Ordering::Acquire);
        let mut value = texts()
            .lock()
            .unwrap()
            .pop()
            .map(|retained| retained.value)
            .unwrap_or_default();
        value.reserve(capacity);
        add_active(value.capacity());
        Self { value, epoch }
    }
    pub fn assign(&mut self, text: crate::text_view::TextView<'_>) {
        self.value.clear();
        let before = self.value.capacity();
        self.value.reserve(text.len());
        add_active(self.value.capacity() - before);
        self.value.push_str(text.prefix);
        self.value.push_str(text.suffix);
    }
}
impl Deref for TextScratch {
    type Target = str;
    fn deref(&self) -> &str {
        &self.value
    }
}
impl Drop for TextScratch {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(self.value.capacity(), Ordering::Relaxed);
        self.value.clear();
        let mut pool = texts().lock().unwrap();
        if self.epoch == EPOCH.load(Ordering::Acquire)
            && pool.len() < crate::cpu_executor::worker_capacity()
            && let Some(reservation) = reserve_buffer(self.value.capacity())
        {
            pool.push(Retained {
                value: std::mem::take(&mut self.value),
                _reservation: reservation,
            });
        }
    }
}
struct ByteScratch {
    value: Vec<u8>,
    epoch: usize,
}
impl Drop for ByteScratch {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(self.value.capacity(), Ordering::Relaxed);
        self.value.clear();
        let mut pool = bytes().lock().unwrap();
        if self.epoch == EPOCH.load(Ordering::Acquire)
            && pool.len() < crate::cpu_executor::worker_capacity()
            && let Some(reservation) = reserve_buffer(self.value.capacity())
        {
            pool.push(Retained {
                value: std::mem::take(&mut self.value),
                _reservation: reservation,
            });
        }
    }
}
pub fn with_bytes<R>(capacity: usize, work: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    let epoch = EPOCH.load(Ordering::Acquire);
    let mut value = bytes()
        .lock()
        .unwrap()
        .pop()
        .map(|retained| retained.value)
        .unwrap_or_default();
    value.reserve(capacity);
    add_active(value.capacity());
    let mut scratch = ByteScratch { value, epoch };
    // Callers reserve the exact upper bound before writing; no unaccounted growth.
    let result = work(&mut scratch.value);
    assert!(scratch.value.len() <= capacity);
    result
}
pub fn clear() {
    EPOCH.fetch_add(1, Ordering::AcqRel);
    texts().lock().unwrap().clear();
    bytes().lock().unwrap().clear();
}
pub fn usage() -> (usize, usize) {
    (ACTIVE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed))
}
