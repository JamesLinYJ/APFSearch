//! Namespace conflicts use the real reconciliation/worker paths. Mount-table
//! semantics are covered independently with synthetic statfs records, avoiding
//! privileged mounts or any change to the user's volumes.
use super::*;

#[test]
fn namespace_errors_remain_distinct_from_io_failure_and_cancellation() {
    for code in [
        libc::EAGAIN,
        libc::ENOENT,
        libc::ENOTDIR,
        libc::ELOOP,
        libc::EACCES,
    ] {
        let error = scanner::MetadataBatchError::from(std::io::Error::from_raw_os_error(code));
        assert!(matches!(
            PreparationError::from(error),
            PreparationError::Retry
        ));
    }
    let error = scanner::MetadataBatchError::from(std::io::Error::from_raw_os_error(libc::EIO));
    assert!(matches!(
        PreparationError::from(error),
        PreparationError::Failure(_)
    ));
    assert!(matches!(
        PreparationError::from(scanner::MetadataBatchError::Cancelled),
        PreparationError::Cancelled
    ));
}

fn linked_fixture() -> (tempfile::TempDir, Arc<SearchEngine>, String, String) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap().join("files");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("first.txt");
    std::fs::write(&file, b"before").unwrap();
    std::fs::hard_link(&file, root.join("alias.txt")).unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index/index.sqlite")).unwrap();
    let root = root.to_str().unwrap().to_owned();
    let file = file.to_str().unwrap().to_owned();
    assert!(
        engine
            .reconcile(
                std::slice::from_ref(&root),
                std::slice::from_ref(&root),
                10,
                true,
                true
            )
            .unwrap()
    );
    (temporary, engine, root, file)
}

#[test]
fn namespace_conflict_preserves_metadata_coverage_cursor_and_old_snapshot() {
    let (_temporary, engine, root, file) = linked_fixture();
    let before = engine.snapshot.load_full();
    let checkpoint = || {
        let store = engine.index_store.lock().unwrap();
        (
            store.get("revision", json!(0)),
            store.get("event_id", json!(0)),
            store.get("uncovered", json!([])),
        )
    };
    let original = checkpoint();
    std::fs::write(&file, b"changed contents").unwrap();
    engine.metadata_conflicts.store(1, Ordering::Relaxed);
    assert_eq!(
        engine
            .reconcile_pass(
                std::slice::from_ref(&file),
                std::slice::from_ref(&root),
                20,
                false,
                false
            )
            .unwrap(),
        ReconciliationOutcome::Retry
    );
    assert_eq!(checkpoint(), original);
    assert!(Arc::ptr_eq(&before, &engine.snapshot.load_full()));
    assert_ne!(engine.state.lock().unwrap()["state"], "error");
    engine.scan_cancel.store(false, Ordering::Relaxed);
    assert!(
        engine
            .reconcile(std::slice::from_ref(&file), &[root], 20, false, false)
            .unwrap()
    );
    let store = engine.index_store.lock().unwrap();
    let rows = store.entries().unwrap();
    assert_eq!(rows.iter().filter(|row| !row.is_dir).count(), 2);
    assert!(
        rows.iter()
            .filter(|row| !row.is_dir)
            .all(|row| row.size == 16)
    );
    assert_eq!(store.get("event_id", json!(0)), 20);
    assert!(
        before
            .visible_entries()
            .filter(|row| !row.is_dir())
            .all(|row| row.size() == 6)
    );
}

#[test]
fn baseline_recheck_retries_namespace_conflicts_without_stopping() {
    let (_temporary, engine, root, file) = linked_fixture();
    std::fs::write(&file, b"changed contents").unwrap();
    engine.metadata_conflicts.store(2, Ordering::Relaxed);
    assert!(
        engine
            .reconcile_baseline(
                std::slice::from_ref(&root),
                std::slice::from_ref(&root),
                20,
                false
            )
            .unwrap()
    );
    assert_eq!(engine.metadata_conflicts.load(Ordering::Relaxed), 0);
    assert_ne!(engine.state.lock().unwrap()["state"], "error");
    assert!(
        engine
            .index_store
            .lock()
            .unwrap()
            .entries()
            .unwrap()
            .iter()
            .filter(|row| !row.is_dir)
            .all(|row| row.size == 16)
    );
}

#[test]
fn active_watcher_requeues_conflicting_events_and_keeps_watching() {
    let (_temporary, engine, root, file) = linked_fixture();
    engine.call(json!({"op":"watch", "roots":[root]}));
    let wait = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !condition() {
            let status = engine.call(json!({"op":"status"}));
            assert_ne!(status["state"], "error", "{status}");
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(&|| engine.state.lock().unwrap()["state"] == "watching");
    engine.metadata_conflicts.store(1, Ordering::Relaxed);
    std::fs::write(&file, b"changed contents").unwrap();
    wait(&|| {
        engine.metadata_conflicts.load(Ordering::Relaxed) == 0
            && engine.state.lock().unwrap()["state"] == "watching"
            && engine
                .snapshot
                .load()
                .visible_entries()
                .filter(|row| !row.is_dir())
                .all(|row| row.size() == 16)
    });
    assert!(engine.active.load(Ordering::Relaxed));
    std::fs::write(&file, b"subsequent independent update").unwrap();
    wait(&|| {
        engine
            .snapshot
            .load()
            .visible_entries()
            .any(|row| row.path() == file && row.size() == 29)
    });
    engine.call(json!({"op":"stop"}));
    if let Some(worker) = engine.worker.lock().unwrap().take() {
        worker.join().unwrap();
    }
}
