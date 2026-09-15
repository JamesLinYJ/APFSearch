//! Snapshot-owned interning for low-cardinality metadata labels.
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Default)]
pub(crate) struct LabelPool {
    values: HashSet<Arc<str>>,
}
impl LabelPool {
    #[cfg(test)]
    fn retaining<'a>(labels: impl IntoIterator<Item = &'a Arc<str>>) -> Self {
        Self {
            values: labels.into_iter().cloned().collect(),
        }
    }
    #[cfg(test)]
    fn intern(pool: &mut Arc<Self>, text: &str) -> Arc<str> {
        if let Some(existing) = pool.values.get(text) {
            return existing.clone();
        }
        let value: Arc<str> = text.into();
        Arc::make_mut(pool).values.insert(value.clone());
        value
    }
    pub(crate) fn share(pool: &mut Arc<Self>, value: &mut Arc<str>) {
        if let Some(existing) = pool.values.get(value.as_ref()) {
            *value = existing.clone();
        } else {
            Arc::make_mut(pool).values.insert(value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_share_storage_without_folding_or_changing_unicode() {
        let mut pool = Arc::new(LabelPool::default());
        let first = LabelPool::intern(&mut pool, "文档");
        let second = LabelPool::intern(&mut pool, "文档");
        assert!(Arc::ptr_eq(&first, &second));
        let upper = LabelPool::intern(&mut pool, "TXT");
        let lower = LabelPool::intern(&mut pool, "txt");
        assert_ne!(upper, lower);
    }

    #[test]
    fn new_labels_do_not_mutate_old_snapshots() {
        let mut old = Arc::new(LabelPool::default());
        let shared = LabelPool::intern(&mut old, "txt");
        let mut new = old.clone();
        let duplicate = LabelPool::intern(&mut new, "txt");
        assert!(Arc::ptr_eq(&old, &new));
        assert!(Arc::ptr_eq(&shared, &duplicate));
        LabelPool::intern(&mut new, "pdf");
        assert!(!old.values.contains("pdf"));
        assert!(new.values.contains("pdf"));
    }

    #[test]
    fn compaction_retains_only_live_labels() {
        let mut pool = Arc::new(LabelPool::default());
        let retained = LabelPool::intern(&mut pool, "txt");
        let discarded = LabelPool::intern(&mut pool, "pdf");
        let weak = Arc::downgrade(&discarded);
        drop(discarded);
        pool = Arc::new(LabelPool::retaining([&retained]));
        assert!(weak.upgrade().is_none());
        assert!(Arc::ptr_eq(pool.values.get("txt").unwrap(), &retained));
    }
}
