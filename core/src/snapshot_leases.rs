//! Bounded read leases keep multi-page operations on one snapshot and one set
//! of preferences even while the live index is publishing newer generations.
use crate::index_store::SearchSnapshot;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
static NEXT_LEASE: AtomicU64 = AtomicU64::new(1);
// Windows retain their visible results and may briefly hold a replacement page.
// Keep their budget separate from the eight concurrent exports/operations so
// ordinary browsing cannot consume the capacity reserved for those operations.
const WINDOW_LEASE_CAPACITY: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseOwner {
    Window,
    Operation,
}

#[derive(Clone)]
pub struct LeaseContext {
    pub snapshot: Arc<SearchSnapshot>,
    pub preferences: Value,
}
struct Record {
    context: LeaseContext,
    deadline: Instant,
    owner: LeaseOwner,
}
pub struct SnapshotLeases {
    records: Mutex<HashMap<String, Record>>,
    lifetime: Duration,
    capacity: usize,
}
impl Default for SnapshotLeases {
    fn default() -> Self {
        Self::new(Duration::from_secs(300), 8)
    }
}
impl SnapshotLeases {
    pub(crate) fn inventory_snapshots(&self) -> (Vec<Arc<SearchSnapshot>>, usize, usize) {
        let records = self.records.lock().unwrap();
        let windows = records
            .values()
            .filter(|record| record.owner == LeaseOwner::Window)
            .count();
        (
            records
                .values()
                .map(|record| record.context.snapshot.clone())
                .collect(),
            windows,
            records.len() - windows,
        )
    }
    fn new(lifetime: Duration, capacity: usize) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            lifetime,
            capacity,
        }
    }
    pub fn retain(
        &self,
        snapshot: Arc<SearchSnapshot>,
        preferences: Value,
    ) -> Result<String, String> {
        self.retain_at(snapshot, preferences, Instant::now())
    }
    pub fn retain_window(
        &self,
        snapshot: Arc<SearchSnapshot>,
        preferences: Value,
    ) -> Result<String, String> {
        self.retain_owned_at(snapshot, preferences, LeaseOwner::Window, Instant::now())
    }
    fn retain_at(
        &self,
        snapshot: Arc<SearchSnapshot>,
        preferences: Value,
        now: Instant,
    ) -> Result<String, String> {
        self.retain_owned_at(snapshot, preferences, LeaseOwner::Operation, now)
    }
    fn retain_owned_at(
        &self,
        snapshot: Arc<SearchSnapshot>,
        preferences: Value,
        owner: LeaseOwner,
        now: Instant,
    ) -> Result<String, String> {
        self.discard_expired_at(now);
        let mut records = self.records.lock().unwrap();
        let capacity = match owner {
            LeaseOwner::Window => WINDOW_LEASE_CAPACITY,
            LeaseOwner::Operation => self.capacity,
        };
        if records
            .values()
            .filter(|record| record.owner == owner)
            .count()
            >= capacity
        {
            return Err(
                "Too many active snapshot leases; finish or cancel another operation".into(),
            );
        }
        let id = format!(
            "{}-{}-{}",
            std::process::id(),
            snapshot.generation,
            NEXT_LEASE.fetch_add(1, Ordering::Relaxed)
        );
        records.insert(
            id.clone(),
            Record {
                context: LeaseContext {
                    snapshot,
                    preferences,
                },
                deadline: now + self.lifetime,
                owner,
            },
        );
        Ok(id)
    }
    pub fn get(&self, id: &str) -> Result<LeaseContext, String> {
        self.get_at(id, Instant::now())
    }
    fn get_at(&self, id: &str, now: Instant) -> Result<LeaseContext, String> {
        self.discard_expired_at(now);
        let mut records = self.records.lock().unwrap();
        let record = records
            .get_mut(id)
            .ok_or("SearchSnapshot lease expired or was released; restart the operation")?;
        // Reading a page renews the lease, so a long but active export does not
        // expire halfway through. Abandoned leases are reclaimed on next access.
        record.deadline = now + self.lifetime;
        Ok(record.context.clone())
    }
    /// Reclaim abandoned expired records before a structural rebuild. Active
    /// leases and already returned reader Arcs keep their existing guarantees.
    pub(crate) fn discard_expired(&self) -> usize {
        self.discard_expired_at(Instant::now())
    }
    fn discard_expired_at(&self, now: Instant) -> usize {
        let expired: Vec<_> = {
            let mut records = self.records.lock().unwrap();
            let ids: Vec<_> = records
                .iter()
                .filter(|(_, record)| record.deadline <= now)
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| records.remove(&id))
                .collect()
        };
        let count = expired.len();
        // Dropping a snapshot can release large posting maps. Keep that work
        // outside the lease lock so active readers can renew concurrently.
        drop(expired);
        count
    }
    pub fn release(&self, id: &str) -> bool {
        let removed = { self.records.lock().unwrap().remove(id) };
        removed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn snapshot(generation: u64) -> Arc<SearchSnapshot> {
        Arc::new(SearchSnapshot::new(vec![], generation))
    }
    #[test]
    fn leases_preserve_snapshot_and_preferences_and_release_capacity() {
        let leases = SnapshotLeases::new(Duration::from_secs(300), 1);
        let first = snapshot(1);
        let id = leases
            .retain(first.clone(), json!({"macros":{"docs":"ext:pdf"}}))
            .unwrap();
        assert!(leases.retain(snapshot(7), json!({})).is_err());
        let held = leases.get(&id).unwrap();
        assert!(Arc::ptr_eq(&held.snapshot, &first));
        assert_eq!(held.preferences["macros"]["docs"], "ext:pdf");
        assert!(leases.release(&id));
        assert!(leases.get(&id).is_err());
        assert!(leases.retain(snapshot(7), json!({})).is_ok());
        assert_eq!(held.snapshot.generation, 1); // A running reader owns its Arc.
    }
    #[test]
    fn active_reads_renew_but_abandoned_leases_expire() {
        let leases = SnapshotLeases::new(Duration::from_secs(10), 1);
        let start = Instant::now();
        let id = leases.retain_at(snapshot(1), json!({}), start).unwrap();
        assert!(leases.get_at(&id, start + Duration::from_secs(9)).is_ok());
        assert!(leases.get_at(&id, start + Duration::from_secs(18)).is_ok());
        assert!(leases.get_at(&id, start + Duration::from_secs(29)).is_err());
        assert!(
            leases
                .retain_at(snapshot(2), json!({}), start + Duration::from_secs(29))
                .is_ok()
        );
    }
    #[test]
    fn window_capacity_is_bounded_and_does_not_consume_operation_slots() {
        let leases = SnapshotLeases::default();
        let shared = snapshot(1);
        let windows: Vec<_> = (0..WINDOW_LEASE_CAPACITY)
            .map(|index| {
                leases
                    .retain_window(shared.clone(), json!({"window": index}))
                    .unwrap()
            })
            .collect();
        assert!(leases.retain_window(shared.clone(), json!({})).is_err());
        let operations: Vec<_> = (0..8)
            .map(|_| leases.retain(shared.clone(), json!({})).unwrap())
            .collect();
        assert!(leases.retain(shared.clone(), json!({})).is_err());
        for (index, token) in windows.iter().enumerate() {
            let held = leases.get(token).unwrap();
            assert!(Arc::ptr_eq(&held.snapshot, &shared));
            assert_eq!(held.preferences["window"], index);
        }
        assert!(leases.release(&windows[0]));
        assert!(leases.retain_window(snapshot(2), json!({})).is_ok());
        assert!(leases.retain(shared.clone(), json!({})).is_err());
        assert!(leases.release(&operations[0]));
        assert!(leases.retain(shared, json!({})).is_ok());
    }
    #[test]
    fn abandoned_window_leases_expire_without_invalidating_active_readers() {
        let leases = SnapshotLeases::new(Duration::from_secs(10), 1);
        let start = Instant::now();
        let token = leases
            .retain_owned_at(snapshot(7), json!({}), LeaseOwner::Window, start)
            .unwrap();
        let reader = leases.get_at(&token, start).unwrap();
        assert_eq!(
            leases.discard_expired_at(start + Duration::from_secs(11)),
            1
        );
        assert!(
            leases
                .get_at(&token, start + Duration::from_secs(11))
                .is_err()
        );
        assert_eq!(reader.snapshot.generation, 7);
    }
    #[test]
    fn rebuild_cleanup_reclaims_only_expired_records_and_preserves_readers() {
        let leases = SnapshotLeases::new(Duration::from_secs(10), 2);
        let start = Instant::now();
        let first = snapshot(1);
        let first_weak = Arc::downgrade(&first);
        let first_id = leases.retain_at(first, json!({}), start).unwrap();
        let reader = leases
            .get_at(&first_id, start + Duration::from_secs(1))
            .unwrap();
        let active = snapshot(2);
        let active_weak = Arc::downgrade(&active);
        let active_id = leases
            .retain_at(active, json!({}), start + Duration::from_secs(8))
            .unwrap();
        assert_eq!(
            leases.discard_expired_at(start + Duration::from_secs(12)),
            1
        );
        assert!(
            first_weak.upgrade().is_some(),
            "inflight reader owns its snapshot"
        );
        assert!(
            leases
                .get_at(&active_id, start + Duration::from_secs(12))
                .is_ok()
        );
        assert!(active_weak.upgrade().is_some());
        assert_eq!(
            leases.discard_expired_at(start + Duration::from_secs(12)),
            0
        );
        drop(reader);
        assert!(
            first_weak.upgrade().is_none(),
            "expired lease no longer retains data"
        );
        assert!(
            leases
                .get_at(&first_id, start + Duration::from_secs(12))
                .is_err()
        );
    }
}
