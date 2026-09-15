use super::*;
use crate::entry_table::FileEntry;

struct CancelAtCommit {
    engine: Arc<SearchEngine>,
    stop_worker: bool,
    fired: AtomicBool,
}
unsafe extern "C" fn cancel_at_commit(
    event: u32,
    context: *mut c_void,
    _statement: *mut c_void,
    sql: *mut c_void,
) -> i32 {
    if event == rusqlite::ffi::SQLITE_TRACE_STMT && !sql.is_null() {
        // The boxed context and SQLite statement string remain valid until the
        // trace is removed after this synchronous reconciliation.
        let context = unsafe { &*context.cast::<CancelAtCommit>() };
        let sql = unsafe { CStr::from_ptr(sql.cast()) };
        if sql.to_bytes() == b"COMMIT" && !context.fired.swap(true, Ordering::Relaxed) {
            context.engine.scan_cancel.store(true, Ordering::Relaxed);
            if context.stop_worker {
                context.engine.stop.store(true, Ordering::Relaxed);
            }
        }
    }
    0
}

#[test]
fn stop_after_committed_batch_defers_publication_but_preserves_recoverable_changes() {
    for stop_worker in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("file.txt"), "original").unwrap();
        let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let db = temp.path().join("index/index.sqlite");
        let engine = SearchEngine::open(&db).unwrap();
        engine.call(json!({"op":"watch","roots":[root]}));
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let status = engine.call(json!({"op":"status"}));
            assert_ne!(status["state"], "error", "{status}");
            if status["state"] == "watching" {
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            std::thread::sleep(Duration::from_millis(20));
        }
        engine.call(json!({"op":"stop"}));
        if let Some(worker) = engine.worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
        let before = engine.snapshot.load_full();
        let old_root = before
            .visible_entries()
            .find(|file| file.path() == root)
            .unwrap();
        let old_modified_ns = old_root.modified_ns();
        // The root row is the scanner's first batch. A deterministic timestamp
        // change guarantees that the traced COMMIT contains a real file update.
        let new_time =
            std::time::UNIX_EPOCH + Duration::from_secs(old_root.modified() as u64 + 100);
        std::fs::File::open(&root)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(new_time))
            .unwrap();
        let expected = scanner::stat_entry(&root).unwrap();
        assert_ne!(old_modified_ns, expected.modified_ns);
        let (cursor, proof, uncovered, revision) = {
            let store = engine.index_store.lock().unwrap();
            let proof = store.get("scan_resume_proof", Value::Null);
            assert!(!proof.is_null());
            (
                store.get("event_id", json!(0)).as_u64().unwrap(),
                proof,
                store.get("uncovered", json!([])),
                store.get("revision", json!(0)).as_u64().unwrap(),
            )
        };
        let cache_path = db.with_extension("snapshot.bin");
        let cache_before = std::fs::read(&cache_path).unwrap();
        let mut trace = Box::new(CancelAtCommit {
            engine: engine.clone(),
            stop_worker,
            fired: AtomicBool::new(false),
        });
        engine.stop.store(false, Ordering::Relaxed);
        engine.scan_cancel.store(false, Ordering::Relaxed);
        {
            let store = engine.index_store.lock().unwrap();
            unsafe {
                assert_eq!(
                    rusqlite::ffi::sqlite3_trace_v2(
                        store.connection.handle(),
                        rusqlite::ffi::SQLITE_TRACE_STMT,
                        Some(cancel_at_commit),
                        (&mut *trace as *mut CancelAtCommit).cast()
                    ),
                    rusqlite::ffi::SQLITE_OK
                );
            }
        }
        let completed = engine.reconcile(
            std::slice::from_ref(&root),
            std::slice::from_ref(&root),
            cursor + 1,
            true,
            false,
        );
        {
            let store = engine.index_store.lock().unwrap();
            unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    store.connection.handle(),
                    0,
                    None,
                    std::ptr::null_mut(),
                );
            }
        }
        assert!(!completed.unwrap());
        assert!(trace.fired.load(Ordering::Relaxed));
        {
            let store = engine.index_store.lock().unwrap();
            assert_eq!(store.get("event_id", json!(0)), cursor);
            assert_eq!(store.get("scan_resume_proof", Value::Null), proof);
            assert_eq!(store.get("uncovered", json!([])), uncovered);
            assert!(store.get("revision", json!(0)).as_u64().unwrap() > revision);
            assert_eq!(
                store
                    .connection
                    .query_row(
                        "SELECT modified_ns FROM files WHERE path=?1",
                        [&root],
                        |row| row.get::<_, i64>(0)
                    )
                    .unwrap(),
                expected.modified_ns
            );
        }
        assert_eq!(std::fs::read(&cache_path).unwrap(), cache_before);
        if stop_worker {
            assert!(Arc::ptr_eq(&before, &engine.snapshot.load_full()));
            assert_eq!(engine.published_revision.load(Ordering::Relaxed), revision);
            assert_eq!(engine.generation.load(Ordering::Relaxed), before.generation);
            assert!(
                engine
                    .index_store
                    .lock()
                    .unwrap()
                    .connection
                    .query_row("SELECT count(*) FROM snapshot_changes", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap()
                    > 0
            );
        } else {
            assert!(
                engine.snapshot.load().generation > before.generation,
                "scan cancellation alone retains normal publication behavior"
            );
        }
        engine.stop.store(false, Ordering::Relaxed);
        engine.scan_cancel.store(false, Ordering::Relaxed);
        assert!(
            engine
                .reconcile(
                    std::slice::from_ref(&root),
                    std::slice::from_ref(&root),
                    cursor + 1,
                    true,
                    false
                )
                .unwrap()
        );
        let after = engine.snapshot.load_full();
        assert!(after.generation > before.generation);
        assert_eq!(
            after
                .visible_entries()
                .find(|file| file.path() == root)
                .unwrap()
                .modified_ns(),
            expected.modified_ns
        );
        assert_eq!(
            before
                .visible_entries()
                .find(|file| file.path() == root)
                .unwrap()
                .modified_ns(),
            old_modified_ns
        );
        assert_eq!(
            engine.index_store.lock().unwrap().get("event_id", json!(0)),
            cursor + 1
        );
    }
}
