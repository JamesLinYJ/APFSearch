//! Explicit, allocation-identity-deduplicated storage diagnostics. These are
//! capacity/payload estimates, not allocator measurements or physical footprint.
//! Never run this traversal as part of ordinary status polling.
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
pub(crate) struct Inventory {
    allocations: HashMap<(usize, &'static str), (usize, u8)>,
    owner: u8,
}
impl Inventory {
    pub(crate) fn owner(&mut self, current: bool) {
        self.owner = if current { 1 } else { 2 };
    }
    pub(crate) fn retired_owner(&mut self) {
        self.owner = 4;
    }
    pub(crate) fn record(&mut self, category: &'static str, identity: usize, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let item = self
            .allocations
            .entry((identity, category))
            .or_insert((bytes, 0));
        debug_assert_eq!(item.0, bytes, "Immutable shared allocation changed size");
        item.1 |= self.owner;
    }
    pub(crate) fn mapping(&mut self, mapping: &std::sync::Arc<memmap2::Mmap>) {
        self.record(
            "mapped_file_bytes",
            std::sync::Arc::as_ptr(mapping) as usize,
            mapping.len(),
        );
    }
    pub(crate) fn bitmap(&mut self, bitmap: &roaring::RoaringBitmap) {
        let statistics = bitmap.statistics();
        // Count the defined Roaring representation, not statistics.n_bytes_*:
        // roaring 0.11.5 reports bitset capacity in bits and array slots as u32
        // even though the arrays contain u16. This is live payload, excluding
        // array spare capacity and container/header allocations.
        self.record(
            "bitmap_live_payload_bytes",
            bitmap as *const _ as usize,
            statistics.n_values_array_containers as usize * std::mem::size_of::<u16>()
                + statistics.n_bitset_containers as usize * (65536 / 8)
                + statistics.n_bytes_run_containers as usize,
        );
    }
    pub(crate) fn report(self) -> Value {
        let mut categories = BTreeMap::<_, [usize; 7]>::new();
        for ((_, category), (bytes, owners)) in self.allocations {
            categories.entry(category).or_default()[owners as usize - 1] += bytes;
        }
        let categories: BTreeMap<_, _> = categories.into_iter().map(|(category, bytes)|
            (category, json!({"current_only":bytes[0],"historical_only":bytes[1],"reclamation_only":bytes[3],"shared":bytes[2]+bytes[4]+bytes[5]+bytes[6],"reclamation_retained":bytes[3..].iter().sum::<usize>(),"total":bytes.iter().sum::<usize>()}))).collect();
        json!({"scope":"Allocation identities are deduplicated across snapshots. Capacities and live payload estimates exclude allocator slack, stacks and untracked transient allocations. Mapped file length is not resident memory.","categories":categories,"allocator":allocator_usage()})
    }
}

pub(crate) fn json_heap(value: &Value) -> usize {
    match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => {
            values.capacity() * std::mem::size_of::<Value>()
                + values.iter().map(json_heap).sum::<usize>()
        }
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                key.capacity() + std::mem::size_of::<(String, Value)>() + json_heap(value)
            })
            .sum(),
        _ => 0,
    }
}

#[cfg(target_os = "macos")]
fn allocator_usage() -> Value {
    #[repr(C)]
    #[derive(Default)]
    struct Statistics {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, statistics: *mut Statistics);
    }
    let mut statistics = Statistics::default();
    // SAFETY: public Darwin malloc_statistics_t ABI, writable aligned storage;
    // a null zone is documented to aggregate all allocator zones.
    unsafe {
        malloc_zone_statistics(std::ptr::null_mut(), &mut statistics);
    }
    json!({"blocks_in_use":statistics.blocks_in_use,"in_use_bytes":statistics.size_in_use,
        "reserved_bytes":statistics.size_allocated,"reserved_not_in_use_bytes":statistics.size_allocated.saturating_sub(statistics.size_in_use)})
}
#[cfg(not(target_os = "macos"))]
fn allocator_usage() -> Value {
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_deduplication_distinguishes_current_old_and_shared_allocations() {
        let mut inventory = Inventory::default();
        inventory.owner(true);
        inventory.record("text_bytes", 10, 100);
        inventory.record("text_bytes", 10, 100);
        inventory.record("text_bytes", 11, 200);
        inventory.owner(false);
        inventory.record("text_bytes", 10, 100);
        inventory.record("text_bytes", 12, 300);
        assert_eq!(
            inventory.report()["categories"]["text_bytes"],
            json!({"current_only":200,"historical_only":300,"shared":100,"total":600,"reclamation_only":0,"reclamation_retained":0})
        );
    }
    #[test]
    fn structural_inventory_is_explicit_and_status_stays_lightweight() {
        let directory = tempfile::tempdir().unwrap();
        let engine = crate::SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
        let ordinary = engine.call(json!({"op":"status"}));
        assert!(ordinary.get("memory_inventory").is_none());
        let diagnostic = engine.call(json!({"op":"status", "diagnostics":"memory"}));
        assert_eq!(diagnostic["success"], true);
        assert_eq!(diagnostic["memory_inventory"]["snapshot_count"], 1);
        assert_eq!(diagnostic["memory_inventory"]["window_leases"], 0);
        assert!(diagnostic["memory_inventory"]["categories"].is_object());
        assert!(
            engine
                .call(json!({"op":"status"}))
                .get("memory_inventory")
                .is_none()
        );
    }
}
