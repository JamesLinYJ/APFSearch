//! Optimistic metadata preparation. No filesystem callback runs under the store
//! mutex or write transaction. The existing atomic alias commit consumes only
//! observations collected against an unchanged database and mount revision.
use super::*;
use crate::index_store::{LinkVerification, LinkVerifier, VerifiedFileObjects};
use crate::scanner::ScannedFile;
use roaring::RoaringTreemap;

impl SearchEngine {
    pub(crate) fn observe_prepared_batch(
        &self,
        entries: &[ScannedFile],
        mut observed: Option<&mut RoaringTreemap>,
        verifier: Option<&mut LinkVerifier<'_>>,
        tracker: Option<&mut VerifiedFileObjects>,
    ) -> Result<usize, String> {
        let Some(verifier) = verifier else {
            let mut store = self.index_store.lock().unwrap();
            let _timer = resource_metrics::Timer::new(&resource_metrics::COMMIT_NS);
            return store.observe_batch(entries, observed, None, None);
        };
        let tracker = tracker.ok_or("Missing link verification context")?;
        for _attempt in 0..8 {
            if self.scan_cancel.load(Ordering::Relaxed) {
                verifier(Vec::new())?;
                return Err("Metadata preparation cancelled".into());
            }
            let scope = scanner::mount_scope().map_err(|error| error.to_string())?;
            tracker.bind_mount(scope.identity());
            let revision = self.index_store.lock().unwrap().get("revision", json!(0));
            let mut all_entries = entries.to_vec();
            let mut available = HashMap::<String, ScannedFile>::new();
            let mut removed = HashSet::new();
            let mut unavailable = HashSet::new();
            let mut visited = HashSet::new();
            let mut conflict = false;
            let preparation = resource_metrics::Timer::new(&resource_metrics::PREPARATION_NS);
            loop {
                let paths = {
                    let store = self.index_store.lock().unwrap();
                    if store.get("revision", json!(0)) != revision {
                        conflict = true;
                        break;
                    }
                    store.verification_paths(&all_entries, tracker)?
                };
                let pending: Vec<_> = paths
                    .into_iter()
                    .filter(|path| visited.insert(path.clone()))
                    .collect();
                if pending.is_empty() {
                    break;
                }
                let verified = verifier(pending)?;
                removed.extend(verified.removed);
                unavailable.extend(verified.unavailable);
                for file in verified.entries {
                    available.insert(file.path.clone(), file.clone());
                    all_entries.push(file);
                }
            }
            drop(preparation);
            if self.scan_cancel.load(Ordering::Relaxed) {
                verifier(Vec::new())?;
                return Err("Metadata preparation cancelled".into());
            }
            if conflict || !scope.still_current().map_err(|error| error.to_string())? {
                resource_metrics::PREPARATION_RETRIES.fetch_add(1, Ordering::Relaxed);
                std::thread::yield_now();
                continue;
            }
            let mut store = self.index_store.lock().unwrap();
            if store.get("revision", json!(0)) != revision {
                resource_metrics::PREPARATION_RETRIES.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let mut staged_observed = RoaringTreemap::new();
            let _commit = resource_metrics::Timer::new(&resource_metrics::COMMIT_NS);
            let changed = store.observe_batch(
                entries,
                observed.as_ref().map(|_| &mut staged_observed),
                Some(&mut |paths| {
                    let mut result = LinkVerification::default();
                    for path in paths {
                        if let Some(file) = available.get(&path) {
                            result.entries.push(file.clone());
                        } else if removed.contains(&path) {
                            result.removed.push(path);
                        } else if unavailable.contains(&path) {
                            result.unavailable.push(path);
                        } else {
                            return Err("Incomplete prepared alias closure".into());
                        }
                    }
                    Ok(result)
                }),
                Some(tracker),
            )?;
            if let Some(observed) = observed.as_deref_mut() {
                *observed |= staged_observed;
            }
            return Ok(changed);
        }
        Err(
            "Index changed repeatedly during metadata preparation; event progress was not advanced"
                .into(),
        )
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn filesystem_verification_releases_store_and_retries_changed_revision() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let first = root.join("first.txt");
        let alias = root.join("alias.txt");
        std::fs::write(&first, b"before").unwrap();
        std::fs::hard_link(&first, &alias).unwrap();
        let engine = SearchEngine::open(&root.join("index/index.sqlite")).unwrap();
        let paths = [first.to_str().unwrap(), alias.to_str().unwrap()];
        let original: Vec<_> = paths
            .iter()
            .map(|path| scanner::stat_entry(path).unwrap())
            .collect();
        engine
            .index_store
            .lock()
            .unwrap()
            .batch(&original, 1)
            .unwrap();
        std::fs::write(&first, b"after update").unwrap();
        let changed = scanner::stat_entry(paths[0]).unwrap();
        let mut calls = 0;
        let mut verify = |paths: Vec<String>| {
            // A verifier must be able to acquire the database mutex: metadata
            // latency must not hold up a query or status reader.
            let store = engine
                .index_store
                .try_lock()
                .expect("Filesystem callback holds store");
            assert!(store.connection.is_autocommit());
            if calls == 0 {
                let revision = store.get("revision", json!(0)).as_u64().unwrap();
                store.set("revision", &json!(revision + 1)).unwrap();
            }
            calls += 1;
            drop(store);
            paths
                .iter()
                .map(|path| scanner::stat_entry(path))
                .collect::<Result<Vec<_>, _>>()
                .map(LinkVerification::from)
        };
        engine
            .observe_prepared_batch(
                &[changed],
                None,
                Some(&mut verify),
                Some(&mut VerifiedFileObjects::default()),
            )
            .unwrap();
        assert!(calls >= 2, "Revision conflict must recollect observations");
        let rows = engine.index_store.lock().unwrap().entries().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|row| row.size == b"after update".len() as u64)
        );
    }
}
