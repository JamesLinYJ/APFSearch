//! Byte-budgeted query products with allocation-free borrowed lookup.
//! Hits lock one shard. Admission and reclamation share a cold-path ledger;
//! active callers retain their Arcs independently of cache eviction.
use crate::{index_store::ResultPage, result_order::ResultOrder};
use hashlink::{LinkedHashMap, linked_hash_map::RawEntryMut};
use roaring::RoaringBitmap;
use std::{
    collections::{HashMap, VecDeque, hash_map::RandomState},
    hash::{BuildHasher, Hash, Hasher},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

const SHARDS: usize = 8;
const MAX_ENTRIES: usize = 512;

#[derive(Clone, PartialEq, Eq)]
enum Key {
    Page(String),
    Matches(String),
    Order(ResultOrder),
    Hierarchy,
}
#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum KeyRef<'a> {
    Page(&'a str),
    Matches(&'a str),
    Order(&'a ResultOrder),
    Hierarchy,
}
impl Key {
    fn borrowed(&self) -> KeyRef<'_> {
        match self {
            Self::Page(key) => KeyRef::Page(key),
            Self::Matches(key) => KeyRef::Matches(key),
            Self::Order(key) => KeyRef::Order(key),
            Self::Hierarchy => KeyRef::Hierarchy,
        }
    }
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Page(key) | Self::Matches(key) => key.capacity(),
            Self::Order(key) => key.heap_bytes(),
            Self::Hierarchy => 0,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct OwnedKey {
    owner: u64,
    key: Key,
}
#[derive(Hash, PartialEq, Eq)]
struct Lookup<'a> {
    owner: u64,
    key: KeyRef<'a>,
}
impl OwnedKey {
    fn borrowed(&self) -> Lookup<'_> {
        Lookup {
            owner: self.owner,
            key: self.key.borrowed(),
        }
    }
}
impl Hash for OwnedKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.borrowed().hash(state);
    }
}

#[derive(Clone)]
enum Product {
    Page(Arc<ResultPage>),
    Matches(Arc<RoaringBitmap>),
    Order(Arc<crate::slot_order::SlotOrder>),
    Hierarchy(Arc<crate::relations::Hierarchy>),
}
impl Product {
    fn allocations(&self) -> Vec<((u8, usize), usize)> {
        let arc_counters = 2 * std::mem::size_of::<AtomicUsize>();
        let single = match self {
            Self::Hierarchy(value) => (
                (3, Arc::as_ptr(value) as usize),
                value.heap_bytes() + arc_counters,
            ),
            Self::Page(value) => (
                (0, Arc::as_ptr(value) as usize),
                std::mem::size_of::<ResultPage>()
                    + value.slots.capacity() * std::mem::size_of::<u32>()
                    + arc_counters,
            ),
            Self::Order(value) => return value.allocations(),
            Self::Matches(value) => {
                let stats = value.statistics();
                // Roaring exposes payload statistics, not allocator capacity.
                // Reserve container/header space conservatively; this is an
                // eviction estimate, never a claim about RSS or physical pages.
                const CONTAINER_RESERVE: usize = 64;
                (
                    (2, Arc::as_ptr(value) as usize),
                    std::mem::size_of::<RoaringBitmap>()
                        + arc_counters
                        + stats.n_containers as usize * CONTAINER_RESERVE
                        + (stats.n_bytes_array_containers
                            + stats.n_bytes_run_containers
                            + stats.n_bytes_bitset_containers) as usize,
                )
            }
        };
        vec![single]
    }
}
struct Entry {
    product: Product,
    allocations: Vec<((u8, usize), usize)>,
    overhead: usize,
    touched: Instant,
}
type Shard = LinkedHashMap<OwnedKey, Entry, RandomState>;
#[derive(Default)]
struct Ledger {
    allocations: HashMap<(u8, usize), (usize, usize)>,
    bytes: usize,
    entries: usize,
}
impl Ledger {
    fn add(&mut self, entry: &Entry) {
        for &(identity, bytes) in &entry.allocations {
            let allocation = self.allocations.entry(identity).or_insert((bytes, 0));
            if allocation.1 == 0 {
                self.bytes += bytes;
            }
            allocation.1 += 1;
        }
        self.bytes += entry.overhead;
        self.entries += 1;
    }
    fn remove(&mut self, entry: &Entry) {
        self.bytes -= entry.overhead;
        self.entries -= 1;
        for &(identity, _) in &entry.allocations {
            let (bytes, references) = self.allocations.get_mut(&identity).unwrap();
            *references -= 1;
            if *references == 0 {
                self.bytes -= *bytes;
                self.allocations.remove(&identity);
            }
        }
    }
}

struct Cache {
    shards: [Mutex<Shard>; SHARDS],
    hasher: RandomState,
    ledger: Mutex<Ledger>,
    estimated_bytes: AtomicUsize,
    reserved_bytes: AtomicUsize,
    limit: usize,
}
impl Cache {
    fn new(limit: usize) -> Self {
        let hasher = RandomState::new();
        let cache = Self {
            shards: std::array::from_fn(|_| Mutex::new(LinkedHashMap::with_hasher(hasher.clone()))),
            hasher,
            ledger: Mutex::new(Ledger::default()),
            estimated_bytes: AtomicUsize::new(0),
            reserved_bytes: AtomicUsize::new(0),
            limit,
        };
        // Some platforms allocate mutex backing on first lock. Initialize it
        // here so even the first lookup of an empty shard needs no allocation.
        for shard in &cache.shards {
            drop(shard.lock().unwrap());
        }
        cache
    }
    fn get(&self, owner: u64, key: KeyRef<'_>) -> Option<Product> {
        let lookup = Lookup { owner, key };
        let hash = self.hasher.hash_one(&lookup);
        let mut shard = self.shards[hash as usize % SHARDS].lock().unwrap();
        match shard
            .raw_entry_mut()
            .from_hash(hash, |stored| stored.borrowed() == lookup)
        {
            RawEntryMut::Occupied(mut entry) => {
                entry.to_back();
                entry.get_mut().touched = Instant::now();
                Some(entry.get().product.clone())
            }
            RawEntryMut::Vacant(_) => None,
        }
    }
    fn insert(&self, owner: u64, key: Key, product: Product) {
        // Measure outside all locks: bitmap statistics can walk many containers.
        let allocations = product.allocations();
        let bytes = allocations.iter().map(|(_, bytes)| *bytes).sum::<usize>();
        // Include linked nodes, hash-table slack and ledger bookkeeping. The
        // allocator's own size classes and resident pages remain unmeasured.
        let bookkeeping = 2
            * (std::mem::size_of::<OwnedKey>()
                + std::mem::size_of::<Entry>()
                + 4 * std::mem::size_of::<usize>());
        let overhead = bookkeeping
            + key.heap_bytes()
            + allocations.capacity() * std::mem::size_of::<((u8, usize), usize)>();
        if bytes.saturating_add(overhead) > self.limit {
            return;
        }
        let key = OwnedKey { owner, key };
        let shard_index = self.hasher.hash_one(&key) as usize % SHARDS;
        let mut retired = Vec::new();
        {
            // All mutations take ledger before shard; hits never take ledger.
            let mut ledger = self.ledger.lock().unwrap();
            {
                let mut shard = self.shards[shard_index].lock().unwrap();
                if let Some(previous) = shard.remove_entry(&key) {
                    ledger.remove(&previous.1);
                    retired.push(previous);
                }
                let entry = Entry {
                    product,
                    allocations,
                    overhead,
                    touched: Instant::now(),
                };
                ledger.add(&entry);
                shard.insert(key, entry);
            }
            while ledger
                .bytes
                .saturating_add(self.reserved_bytes.load(Ordering::Relaxed))
                > self.limit
                || ledger.entries > MAX_ENTRIES
            {
                // Compare only the oldest entry in each fixed shard, not every
                // cached key. Concurrent touches may change the exact global
                // victim; each shard still maintains constant-time true LRU.
                let oldest = self
                    .shards
                    .iter()
                    .enumerate()
                    .filter_map(|(index, shard)| {
                        shard
                            .lock()
                            .unwrap()
                            .front()
                            .map(|(_, entry)| (index, entry.touched))
                    })
                    .min_by_key(|(_, touched)| *touched)
                    .map(|(index, _)| index);
                let Some(index) = oldest else {
                    break;
                };
                if let Some(previous) = self.shards[index].lock().unwrap().pop_front() {
                    ledger.remove(&previous.1);
                    retired.push(previous);
                }
            }
            self.estimated_bytes.store(ledger.bytes, Ordering::Relaxed);
        }
        // Destructors of large bitmaps/arrays never run under cache locks.
        drop(retired);
    }
    fn remove_owner(&self, owner: u64) {
        let mut retired = Vec::new();
        {
            let mut ledger = self.ledger.lock().unwrap();
            for shard in &self.shards {
                let mut shard = shard.lock().unwrap();
                // Snapshot disposal is cold; only lookup/touch need O(1).
                let keys: Vec<_> = shard
                    .keys()
                    .filter(|key| key.owner == owner)
                    .cloned()
                    .collect();
                for key in keys {
                    let previous = shard.remove_entry(&key).unwrap();
                    ledger.remove(&previous.1);
                    retired.push(previous);
                }
            }
            self.estimated_bytes.store(ledger.bytes, Ordering::Relaxed);
        }
        drop(retired);
    }
    fn orders(&self, owner: u64) -> VecDeque<(ResultOrder, Arc<crate::slot_order::SlotOrder>)> {
        let mut result = VecDeque::new();
        for shard in &self.shards {
            let shard = shard.lock().unwrap();
            result.extend(shard.iter().filter_map(|(key, entry)| {
                if key.owner == owner
                    && let (Key::Order(order), Product::Order(value)) = (&key.key, &entry.product)
                {
                    Some((order.clone(), value.clone()))
                } else {
                    None
                }
            }));
        }
        result
    }
    fn clear(&self) {
        let mut retired = Vec::with_capacity(SHARDS);
        {
            let mut ledger = self.ledger.lock().unwrap();
            for shard in &self.shards {
                retired.push(std::mem::replace(
                    &mut *shard.lock().unwrap(),
                    LinkedHashMap::with_hasher(self.hasher.clone()),
                ));
            }
            *ledger = Ledger::default();
            self.estimated_bytes.store(0, Ordering::Relaxed);
        }
        drop(retired);
    }
}
fn global() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut memory = 4_u64 * 1024 * 1024 * 1024;
        #[cfg(target_os = "macos")]
        {
            let mut length = std::mem::size_of_val(&memory);
            // The constant sysctl key writes one u64 into this sized buffer.
            let result = unsafe {
                libc::sysctlbyname(
                    c"hw.memsize".as_ptr(),
                    std::ptr::addr_of_mut!(memory).cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if result != 0 || length != 8 {
                memory = 4 * 1024 * 1024 * 1024;
            }
        }
        Cache::new((memory / 64).clamp(64 * 1024 * 1024, 256 * 1024 * 1024) as usize)
    })
}
#[derive(Debug)]
pub(crate) struct DerivedCache {
    owner: u64,
}
impl Default for DerivedCache {
    fn default() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            owner: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
}
impl Drop for DerivedCache {
    fn drop(&mut self) {
        global().remove_owner(self.owner);
    }
}
impl DerivedCache {
    pub(crate) fn hierarchy(&self) -> Option<Arc<crate::relations::Hierarchy>> {
        match global().get(self.owner, KeyRef::Hierarchy)? {
            Product::Hierarchy(value) => Some(value),
            _ => unreachable!(),
        }
    }
    pub(crate) fn insert_hierarchy(&self, value: Arc<crate::relations::Hierarchy>) {
        global().insert(self.owner, Key::Hierarchy, Product::Hierarchy(value));
    }
    pub(crate) fn page(&self, key: &str) -> Option<Arc<ResultPage>> {
        match global().get(self.owner, KeyRef::Page(key))? {
            Product::Page(value) => Some(value),
            _ => unreachable!(),
        }
    }
    pub(crate) fn insert_page(&self, key: String, value: Arc<ResultPage>) {
        global().insert(self.owner, Key::Page(key), Product::Page(value));
    }
    pub(crate) fn matches(&self, key: &str) -> Option<Arc<RoaringBitmap>> {
        match global().get(self.owner, KeyRef::Matches(key))? {
            Product::Matches(value) => Some(value),
            _ => unreachable!(),
        }
    }
    pub(crate) fn insert_matches(&self, key: String, value: Arc<RoaringBitmap>) {
        global().insert(self.owner, Key::Matches(key), Product::Matches(value));
    }
    pub(crate) fn order(&self, key: &ResultOrder) -> Option<Arc<crate::slot_order::SlotOrder>> {
        match global().get(self.owner, KeyRef::Order(key))? {
            Product::Order(value) => Some(value),
            _ => unreachable!(),
        }
    }
    pub(crate) fn insert_order(&self, key: ResultOrder, value: Arc<crate::slot_order::SlotOrder>) {
        global().insert(self.owner, Key::Order(key), Product::Order(value));
    }
    pub(crate) fn orders(&self) -> VecDeque<(ResultOrder, Arc<crate::slot_order::SlotOrder>)> {
        global().orders(self.owner)
    }
    pub(crate) fn from_orders(
        orders: VecDeque<(ResultOrder, Arc<crate::slot_order::SlotOrder>)>,
    ) -> Self {
        let cache = Self::default();
        for (key, value) in orders {
            cache.insert_order(key, value);
        }
        cache
    }
}
pub(crate) fn usage() -> (usize, usize) {
    let cache = global();
    (
        cache.estimated_bytes.load(Ordering::Relaxed)
            + cache.reserved_bytes.load(Ordering::Relaxed),
        cache.limit,
    )
}
pub(crate) fn clear() {
    global().clear();
    crate::query_scratch::clear();
}

pub(crate) struct BufferReservation(usize);
pub(crate) fn reserve_buffer(bytes: usize) -> Option<BufferReservation> {
    let cache = global();
    let ledger = cache.ledger.lock().unwrap();
    let total = ledger
        .bytes
        .checked_add(cache.reserved_bytes.load(Ordering::Relaxed))?
        .checked_add(bytes)?;
    if total > cache.limit {
        return None;
    }
    cache.reserved_bytes.fetch_add(bytes, Ordering::Relaxed);
    Some(BufferReservation(bytes))
}
impl Drop for BufferReservation {
    fn drop(&mut self) {
        global().reserved_bytes.fetch_sub(self.0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };
    thread_local! {
        static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
    }
    struct CountingAllocator;
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;
    fn count_allocation() {
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(value) = count.get() {
                count.set(Some(value + 1));
            }
        });
    }
    // Forward the System allocator contract unchanged. Only the measuring
    // test thread enables counting; concurrent tests do not affect its result.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            count_allocation();
            unsafe { System.alloc(layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            count_allocation();
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            unsafe { System.dealloc(pointer, layout) }
        }
        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            count_allocation();
            unsafe { System.realloc(pointer, layout, size) }
        }
    }
    #[test]
    fn borrowed_hits_and_misses_allocate_nothing() {
        let cache = Cache::new(8192);
        let order = ResultOrder::name();
        cache.insert(
            1,
            Key::Order(order.clone()),
            Product::Order(Arc::new(vec![1, 2].into())),
        );
        cache.insert(
            1,
            Key::Matches("中文 query".into()),
            Product::Matches(Arc::new(RoaringBitmap::new())),
        );
        cache.insert(
            1,
            Key::Page("page".into()),
            Product::Page(Arc::new(ResultPage {
                slots: vec![1],
                total: 1,
                offset: 0,
                anchor_index: None,
            })),
        );
        ALLOCATIONS.set(Some(0));
        for _ in 0..1000 {
            assert!(std::hint::black_box(cache.get(1, KeyRef::Page("page"))).is_some());
            assert!(std::hint::black_box(cache.get(1, KeyRef::Matches("中文 query"))).is_some());
            assert!(std::hint::black_box(cache.get(1, KeyRef::Order(&order))).is_some());
            assert!(std::hint::black_box(cache.get(1, KeyRef::Page("missing"))).is_none());
        }
        let count = ALLOCATIONS.replace(None).unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn recently_used_entries_survive_shard_eviction() {
        let mut cache = Cache::new(8192);
        let keys: Vec<_> = (0..)
            .map(|index| format!("{index:08}"))
            .filter(|key| {
                (cache.hasher.hash_one(Lookup {
                    owner: 1,
                    key: KeyRef::Matches(key),
                }) as usize)
                    .is_multiple_of(SHARDS)
            })
            .take(3)
            .collect();
        let product = || Product::Matches(Arc::new((0..128).collect()));
        cache.insert(1, Key::Matches(keys[0].clone()), product());
        cache.limit = 2 * cache.estimated_bytes.load(Ordering::Relaxed);
        cache.insert(1, Key::Matches(keys[1].clone()), product());
        assert!(cache.get(1, KeyRef::Matches(&keys[0])).is_some());
        cache.insert(1, Key::Matches(keys[2].clone()), product());
        assert!(cache.get(1, KeyRef::Matches(&keys[0])).is_some());
        assert!(cache.get(1, KeyRef::Matches(&keys[1])).is_none());
        assert!(cache.get(1, KeyRef::Matches(&keys[2])).is_some());
        assert!(cache.estimated_bytes.load(Ordering::Relaxed) <= cache.limit);
    }
    #[test]
    fn shared_allocations_are_counted_once_across_generations() {
        let cache = Cache::new(4096);
        let product = Product::Matches(Arc::new((0..128).collect()));
        let allocation_bytes = product
            .allocations()
            .iter()
            .map(|(_, bytes)| *bytes)
            .sum::<usize>();
        cache.insert(1, Key::Matches(String::new()), product.clone());
        let single = cache.estimated_bytes.load(Ordering::Relaxed);
        cache.insert(2, Key::Matches(String::new()), product);
        assert_eq!(
            cache.estimated_bytes.load(Ordering::Relaxed),
            single * 2 - allocation_bytes
        );
        cache.remove_owner(1);
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), single);
        cache.remove_owner(2);
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn order_budget_counts_shared_leaves_once_and_releases_each_owner() {
        let first: crate::slot_order::SlotOrder = (0..16000).collect::<Vec<_>>().into();
        let second = first.updated(&mut vec![6000], &[16001], |a, b| a.cmp(&b));
        let products = [
            Product::Order(Arc::new(first)),
            Product::Order(Arc::new(second)),
        ];
        let mut unique = std::collections::HashMap::new();
        let sum: usize = products
            .iter()
            .flat_map(Product::allocations)
            .map(|(key, bytes)| {
                unique.insert(key, bytes);
                bytes
            })
            .sum();
        let unique_bytes: usize = unique.values().sum();
        assert!(unique_bytes < sum);
        let cache = Cache::new(1024 * 1024);
        for (owner, product) in products.into_iter().enumerate() {
            cache.insert(owner as u64, Key::Order(ResultOrder::name()), product);
        }
        assert_eq!(
            cache
                .ledger
                .lock()
                .unwrap()
                .allocations
                .values()
                .map(|(bytes, _)| bytes)
                .sum::<usize>(),
            unique_bytes
        );
        cache.remove_owner(0);
        assert!(cache.get(1, KeyRef::Order(&ResultOrder::name())).is_some());
        cache.remove_owner(1);
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), 0);
    }
    #[test]
    fn pressure_and_owner_disposal_preserve_active_results() {
        let cache = Cache::new(4096);
        let active = Arc::new((0..128).collect::<RoaringBitmap>());
        cache.insert(
            1,
            Key::Matches("first".into()),
            Product::Matches(active.clone()),
        );
        cache.insert(
            2,
            Key::Matches("second".into()),
            Product::Matches(active.clone()),
        );
        cache.remove_owner(1);
        assert!(cache.get(1, KeyRef::Matches("first")).is_none());
        assert!(cache.get(2, KeyRef::Matches("second")).is_some());
        cache.clear();
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(active.len(), 128);
    }
    #[test]
    fn oversized_products_do_not_displace_useful_cached_results() {
        let cache = Cache::new(1024);
        cache.insert(
            1,
            Key::Matches(String::new()),
            Product::Matches(Arc::new(RoaringBitmap::new())),
        );
        let before = cache.estimated_bytes.load(Ordering::Relaxed);
        cache.insert(
            2,
            Key::Order(ResultOrder::name()),
            Product::Order(Arc::new(vec![0; 1024].into())),
        );
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), before);
        assert!(cache.get(1, KeyRef::Matches("")).is_some());
    }
    #[test]
    fn hits_do_not_acquire_admission_lock_and_all_types_use_borrowed_keys() {
        let cache = Cache::new(4096);
        let order = ResultOrder::name();
        cache.insert(
            1,
            Key::Order(order.clone()),
            Product::Order(Arc::new(vec![1, 2].into())),
        );
        cache.insert(
            2,
            Key::Matches("query".into()),
            Product::Matches(Arc::new(RoaringBitmap::new())),
        );
        let _admission = cache.ledger.lock().unwrap();
        assert!(cache.get(1, KeyRef::Order(&order)).is_some());
        assert!(cache.get(2, KeyRef::Matches("query")).is_some());
        assert!(cache.get(1, KeyRef::Matches("query")).is_none());
    }
    #[test]
    fn concurrent_hits_admission_and_pressure_keep_budget_consistent() {
        let cache = Cache::new(8192);
        std::thread::scope(|scope| {
            for owner in 0..4 {
                let cache = &cache;
                scope.spawn(move || {
                    for index in 0..200 {
                        cache.insert(
                            owner,
                            Key::Matches(index.to_string()),
                            Product::Matches(Arc::new((0..128).collect())),
                        );
                        let _ = cache.get(owner, KeyRef::Matches(&index.to_string()));
                        if index % 37 == 0 {
                            cache.clear();
                        }
                    }
                    cache.remove_owner(owner);
                });
            }
        });
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), 0);
        let ledger = cache.ledger.lock().unwrap();
        assert!(ledger.allocations.is_empty());
        assert_eq!(ledger.entries, 0);
    }
    #[test]
    fn hierarchy_budget_releases_cache_ownership_without_invalidating_readers() {
        let cache = Cache::new(8192);
        let tree = Arc::new(crate::relations::Hierarchy::default());
        let weak = Arc::downgrade(&tree);
        cache.insert(1, Key::Hierarchy, Product::Hierarchy(tree.clone()));
        drop(tree);
        let Product::Hierarchy(reader) = cache.get(1, KeyRef::Hierarchy).unwrap() else {
            panic!("Unexpected cache product")
        };
        cache.clear();
        assert!(weak.upgrade().is_some());
        assert_eq!(cache.estimated_bytes.load(Ordering::Relaxed), 0);
        drop(reader);
        assert!(weak.upgrade().is_none());
        let tiny = Cache::new(1);
        tiny.insert(
            1,
            Key::Hierarchy,
            Product::Hierarchy(Arc::new(crate::relations::Hierarchy::default())),
        );
        assert!(tiny.get(1, KeyRef::Hierarchy).is_none());
    }
}
