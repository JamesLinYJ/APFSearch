use super::*;
use crate::{entry_table::FileEntry, scanner::ScannedFile};

fn row(size: u64) -> ScannedFile {
    ScannedFile {
        path: "/recovery-fixture/item.txt".into(),
        name: "item.txt".into(),
        extension: "txt".into(),
        volume_id: "fixture".into(),
        size,
        file_id: 1,
        ..Default::default()
    }
}

#[test]
fn recovery_read_pins_rows_and_revisions_without_holding_writer_mutex() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    let (read, revision) = {
        let mut store = engine.index_store.lock().unwrap();
        store.batch(&[row(10)], 1).unwrap();
        let revision = store.get("revision", json!(0)).as_u64().unwrap();
        (store.snapshot_read().unwrap(), revision)
    };
    assert_eq!(read.revision, revision);
    // This commit must succeed while the recovery view is alive, and must not
    // change either its revision or streamed rows.
    engine
        .index_store
        .try_lock()
        .unwrap()
        .batch(&[row(20)], 2)
        .unwrap();
    let snapshot = read.build(7).unwrap();
    assert_eq!(snapshot.visible_entries().next().unwrap().size(), 10);
    let store = engine.index_store.lock().unwrap();
    assert!(store.get("revision", json!(0)).as_u64().unwrap() > revision);
    assert_eq!(store.entries().unwrap()[0].size, 20);
    let checkpoint: (i64, i64, i64) = store
        .connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(
        checkpoint,
        (0, 0, 0),
        "Recovery left a WAL read view pinned"
    );
}

#[test]
fn saturated_cpu_recovery_keeps_the_writer_available() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    let read = {
        let mut store = engine.index_store.lock().unwrap();
        store.batch(&[row(10)], 1).unwrap();
        store.snapshot_read().unwrap()
    };
    let cancelled = AtomicBool::new(false);
    let permits: Vec<_> = (0..cpu_executor::worker_capacity())
        .map(|_| cpu_executor::enter_query(&cancelled).unwrap())
        .collect();
    let (entered, started) = std::sync::mpsc::sync_channel(0);
    let (finished, completed) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        entered.send(()).unwrap();
        finished.send(read.build(7)).unwrap();
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    // Packing requires a CPU permit. Writers and admitted content readers must
    // still be able to obtain the store while recovery is waiting for one.
    engine
        .index_store
        .try_lock()
        .expect("Recovery blocked the writer while waiting for CPU")
        .batch(&[row(20)], 2)
        .unwrap();
    assert!(completed.try_recv().is_err());
    drop(permits);
    let snapshot = completed
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
    assert_eq!(snapshot.visible_entries().next().unwrap().size(), 10);
}

#[test]
fn cache_encoding_releases_writer_mutex_and_does_not_acknowledge_newer_data() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&[row(10)], 1)
        .unwrap();
    engine.refresh(false).unwrap();
    let snapshot = engine.snapshot.load_full();
    let revision = engine.published_revision.load(Ordering::Relaxed);
    engine.persist_snapshot_cache_with(&snapshot, revision, |path, snapshot, revision| {
        // Content queries acquire this lock while owning a CPU permit. Encoding
        // must make it available before entering the background CPU budget.
        engine
            .index_store
            .try_lock()
            .expect("Cache publisher holds database mutex")
            .batch(&[row(20)], 2)
            .unwrap();
        snapshot_cache::write(path, snapshot, revision)
    });
    let store = engine.index_store.lock().unwrap();
    assert!(
        store.cache_is_dirty(),
        "Old publication acknowledged a new revision"
    );
    assert_eq!(store.entries().unwrap()[0].size, 20);
    assert_ne!(store.get("revision", json!(0)).as_u64(), Some(revision));
    assert!(engine.state.lock().unwrap().get("cache_error").is_none());
    drop(store);
    engine.refresh(true).unwrap();
    let store = engine.index_store.lock().unwrap();
    let (restored, _) = store
        .cache_read()
        .expect("Latest publication must be recoverable");
    assert_eq!(restored.visible_entries().next().unwrap().size(), 20);
    assert!(!store.cache_is_dirty());
}

#[test]
fn failed_cache_publication_preserves_search_and_pending_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&[row(10)], 1)
        .unwrap();
    engine.refresh(false).unwrap();
    let snapshot = engine.snapshot.load_full();
    let revision = engine.published_revision.load(Ordering::Relaxed);
    engine.persist_snapshot_cache_with(&snapshot, revision, |_, _, _| {
        Err("fixture publication failure".into())
    });
    assert!(engine.index_store.lock().unwrap().cache_is_dirty());
    assert_eq!(
        engine.state.lock().unwrap()["cache_error"],
        "fixture publication failure"
    );
    assert_eq!(engine.call(json!({"op":"query","text":"item"}))["total"], 1);
}

#[test]
fn saturated_cpu_publication_leaves_content_store_available() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    engine
        .index_store
        .lock()
        .unwrap()
        .batch(&[row(10)], 1)
        .unwrap();
    engine.refresh(false).unwrap();
    let snapshot = engine.snapshot.load_full();
    let revision = engine.published_revision.load(Ordering::Relaxed);
    let cancelled = AtomicBool::new(false);
    let permits: Vec<_> = (0..cpu_executor::worker_capacity())
        .map(|_| cpu_executor::enter_query(&cancelled).unwrap())
        .collect();
    let (entered, started) = std::sync::mpsc::sync_channel(0);
    let (finished, completed) = std::sync::mpsc::sync_channel(0);
    let publisher = engine.clone();
    let worker = std::thread::spawn(move || {
        publisher.persist_snapshot_cache_with(&snapshot, revision, |path, snapshot, revision| {
            entered.send(()).unwrap();
            snapshot_cache::write(path, snapshot, revision)
        });
        finished.send(()).unwrap();
    });
    started.recv_timeout(Duration::from_secs(10)).unwrap();
    // The publisher cannot acquire a CPU permit yet. A query that already owns
    // one must nevertheless be able to read content and release its permit.
    assert_eq!(
        engine
            .index_store
            .try_lock()
            .expect("Publisher blocked content reads")
            .content_for("/recovery-fixture/item.txt")
            .unwrap(),
        None
    );
    assert!(completed.try_recv().is_err());
    drop(permits);
    completed.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert!(!engine.index_store.lock().unwrap().cache_is_dirty());
}

#[test]
fn failed_engine_open_returns_owned_structured_error_instead_of_aborting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    std::fs::write(&path, b"not a SQLite database").unwrap();
    let path = CString::new(path.to_str().unwrap()).unwrap();
    let mut error = std::ptr::null_mut();
    // SAFETY: path and output storage live for the call; free the returned
    // allocation exactly once using the matching engine allocator.
    unsafe {
        let handle = apfsearch_engine_open_with_error(path.as_ptr(), &mut error);
        assert!(handle.is_null());
        assert!(!error.is_null());
        let response: Value = serde_json::from_slice(CStr::from_ptr(error).to_bytes()).unwrap();
        assert_eq!(response["success"], false);
        assert_eq!(response["error_code"], "index_open_failed");
        assert!(!response["error"].as_str().unwrap().is_empty());
        apfsearch_engine_free_string(error);
    }
}

#[test]
fn successful_engine_open_clears_error_output() {
    let directory = tempfile::tempdir().unwrap();
    let path = CString::new(directory.path().join("index.sqlite").to_str().unwrap()).unwrap();
    let mut error = std::ptr::dangling_mut::<c_char>();
    // SAFETY: path and output storage are valid; the engine is closed once and
    // no requests or background scan refer to this empty fixture.
    unsafe {
        let handle = apfsearch_engine_open_with_error(path.as_ptr(), &mut error);
        assert!(!handle.is_null());
        assert!(error.is_null());
        apfsearch_engine_close(handle);
    }
}
