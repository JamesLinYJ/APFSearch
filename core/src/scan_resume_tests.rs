use super::*;
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt};

fn await_watching(engine: &Arc<SearchEngine>) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = engine.call(json!({"op":"status"}));
        assert_ne!(status["state"], "error", "{status}");
        if status["state"] == "watching" {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "watcher never caught up: {status}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn stop_join(engine: &Arc<SearchEngine>) {
    engine.call(json!({"op":"stop"}));
    if let Some(worker) = engine.worker.lock().unwrap().take() {
        worker.join().unwrap();
    }
}
fn file_paths(engine: &Arc<SearchEngine>) -> BTreeSet<String> {
    engine.call(json!({"op":"query","text":"","limit":1000}))["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["path"].as_str().unwrap().to_owned())
        .collect()
}
#[test]
fn historical_replay_restores_offline_changes_and_unchanged_startup_does_not_rewrite_rows() {
    let temp = tempfile::Builder::new()
        .prefix("filesearch-resume-")
        .tempdir()
        .unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir_all(root.join("before")).unwrap();
    std::fs::write(root.join("a.txt"), "a").unwrap();
    std::fs::write(root.join("delete.txt"), "delete").unwrap();
    std::fs::write(root.join("before/child.txt"), "child").unwrap();
    let root = root.canonicalize().unwrap();
    let db = temp.path().join("index/index.sqlite");
    let engine = SearchEngine::open(&db).unwrap();
    engine.call(json!({"op":"watch","roots":[root]}));
    let first = await_watching(&engine);
    assert_eq!(first["resumed"], false);
    assert!(first["event_id"].as_u64().unwrap() > 0);
    stop_join(&engine);
    {
        let store = engine.index_store.lock().unwrap();
        assert!(!store.get("scan_resume_proof", Value::Null).is_null());
        store.connection.execute_batch("CREATE TABLE file_writes(id INTEGER); CREATE TRIGGER observe_file_writes AFTER UPDATE ON files BEGIN INSERT INTO file_writes VALUES(new.id); END;").unwrap();
    }
    drop(engine);
    let engine = SearchEngine::open(&db).unwrap();
    let resumed = await_watching(&engine);
    assert_eq!(resumed["resumed"], true, "{resumed}");
    assert_eq!(resumed["scan_processed_entries"], 0);
    assert_eq!(
        resumed["resumed_recheck_roots"],
        json!([]),
        "fixture must not be treated as an auxiliary volume"
    );
    eprintln!(
        "unchanged resume actual reconciliation: {} entries",
        resumed["reconciled_entries"]
    );
    assert_eq!(
        engine
            .index_store
            .lock()
            .unwrap()
            .connection
            .query_row("SELECT count(*) FROM file_writes", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0,
        "unchanged rows must not be rewritten"
    );
    stop_join(&engine);
    drop(engine);
    std::fs::write(root.join("a.txt"), "changed content").unwrap();
    std::fs::remove_file(root.join("delete.txt")).unwrap();
    std::fs::rename(root.join("before"), root.join("after")).unwrap();
    std::fs::write(root.join("new.txt"), "new").unwrap();
    // A lost derived cache must be repaired once after replay, without forcing
    // another filesystem baseline or losing the offline changes.
    std::fs::remove_file(db.with_extension("snapshot.bin")).unwrap();
    let engine = SearchEngine::open(&db).unwrap();
    let resumed = await_watching(&engine);
    assert_eq!(resumed["resumed"], true);
    // FSEvents can finish its history before freshly buffered daemon events.
    let expected: BTreeSet<_> = ["", "a.txt", "after", "after/child.txt", "new.txt"]
        .into_iter()
        .map(|suffix| {
            root.join(suffix)
                .to_string_lossy()
                .trim_end_matches('/')
                .to_owned()
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    while file_paths(&engine) != expected && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(file_paths(&engine), expected);
    let rows = engine.call(json!({"op":"query","text":"a.txt"}));
    assert_eq!(rows["rows"][0]["size"], 15);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some((snapshot, _)) = engine.index_store.lock().unwrap().cache_read() {
            let cached: BTreeSet<_> = snapshot
                .visible_entries()
                .map(|file| file.path.clone())
                .collect();
            if cached == expected {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "missing cache was not repaired after replay"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    stop_join(&engine);
    drop(engine);
    std::fs::set_permissions(root.join("after"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let engine = SearchEngine::open(&db).unwrap();
    assert_eq!(
        await_watching(&engine)["resumed"],
        true,
        "nested permissions use scoped history reconciliation"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.call(json!({"op":"query","text":"child.txt"}))["total"] != 0
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        engine.call(json!({"op":"query","text":"child.txt"}))["total"],
        0
    );
    stop_join(&engine);
    drop(engine);
    // A newly discovered denied scope changes the dynamic probe collection.
    // That must not invalidate the already completed root/journal baseline.
    let engine = SearchEngine::open(&db).unwrap();
    let resumed = await_watching(&engine);
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["resume_reason"], "journal_resume");
    assert_eq!(resumed["scan_processed_entries"], 0);
    assert_eq!(
        resumed["resumed_recheck_roots"],
        json!([root.join("after")])
    );
    stop_join(&engine);
    drop(engine);
    std::fs::set_permissions(root.join("after"), std::fs::Permissions::from_mode(0o755)).unwrap();
}
#[test]
fn invalid_history_requires_a_baseline_but_permission_changes_recheck_only_their_scope() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir_all(root.join("private")).unwrap();
    std::fs::write(root.join("private/secret.txt"), "secret").unwrap();
    let root = root.canonicalize().unwrap();
    let db = temp.path().join("index/index.sqlite");
    let engine = SearchEngine::open(&db).unwrap();
    engine.call(json!({"op":"watch","roots":[root]}));
    await_watching(&engine);
    stop_join(&engine);
    engine
        .index_store
        .lock()
        .unwrap()
        .set("scan_resume_proof", &Value::Null)
        .unwrap();
    drop(engine);
    let engine = SearchEngine::open(&db).unwrap();
    let missing = await_watching(&engine);
    assert_eq!(missing["resumed"], false);
    assert_eq!(missing["resume_reason"], "missing_completed_baseline_proof");
    stop_join(&engine);
    let mut proof = engine
        .index_store
        .lock()
        .unwrap()
        .get("scan_resume_proof", Value::Null);
    proof["journal_uuid"] = json!("invalidated-history");
    engine
        .index_store
        .lock()
        .unwrap()
        .set("scan_resume_proof", &proof)
        .unwrap();
    drop(engine);
    let engine = SearchEngine::open(&db).unwrap();
    let invalid = await_watching(&engine);
    assert_eq!(invalid["resumed"], false);
    assert_eq!(invalid["resume_reason"], "journal_identity_changed");
    stop_join(&engine);
    drop(engine);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let engine = SearchEngine::open(&db).unwrap();
    let status = await_watching(&engine);
    assert_eq!(status["resumed"], true);
    assert_eq!(status["resume_reason"], "journal_resume");
    assert_eq!(status["resumed_recheck_roots"], json!([root]));
    assert_eq!(status["scan_processed_entries"], 0);
    assert_eq!(status["count"], 0);
    assert!(!status["uncovered"].as_array().unwrap().is_empty());
    stop_join(&engine);
    drop(engine);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let engine = SearchEngine::open(&db).unwrap();
    await_watching(&engine);
    assert_eq!(
        engine.call(json!({"op":"query","text":"secret"}))["total"],
        1
    );
    stop_join(&engine);
}

#[test]
fn a_forced_unchanged_scan_writes_no_file_rows_and_deletes_only_missing_members() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    for i in 0..20 {
        std::fs::write(root.join(format!("file-{i}.txt")), "unchanged").unwrap();
    }
    let engine = SearchEngine::open(&temp.path().join("index/index.sqlite")).unwrap();
    let scan = json!({"op":"scan","roots":[root],"watch":false,"wait":true});
    assert_eq!(engine.call(scan.clone())["success"], true);
    engine.index_store.lock().unwrap().connection.execute_batch("CREATE TABLE writes(id INTEGER); CREATE TRIGGER observe_write AFTER UPDATE ON files BEGIN INSERT INTO writes VALUES(new.id); END;").unwrap();
    assert_eq!(engine.call(scan.clone())["success"], true);
    assert_eq!(
        engine
            .index_store
            .lock()
            .unwrap()
            .connection
            .query_row("SELECT count(*) FROM writes", [], |row| row
                .get::<_, usize>(0))
            .unwrap(),
        0
    );
    std::fs::remove_file(root.join("file-3.txt")).unwrap();
    assert_eq!(engine.call(scan)["success"], true);
    assert_eq!(
        engine.call(json!({"op":"query","text":"file-3.txt"}))["total"],
        0
    );
    assert_eq!(
        engine.call(json!({"op":"query","text":"file-4.txt"}))["total"],
        1
    );
    assert_eq!(engine.call(json!({"op":"status"}))["count"], 20);
}

#[test]
fn startup_namespace_proof_does_not_treat_the_data_volume_as_an_auxiliary_disk() {
    use std::os::unix::fs::MetadataExt;
    for path in ["/", "/System/Volumes/Data", "/Users", "/private/tmp"] {
        eprintln!(
            "resume device {path}: dev={} journal={:?}",
            std::fs::symlink_metadata(path).unwrap().dev(),
            file_events::journal_uuid(Path::new(path)).unwrap()
        );
    }
    let inspection = scan_resume::inspect(&["/".into()], &[]).unwrap();
    let mut legacy = serde_json::to_value(&inspection.proof).unwrap();
    legacy.as_object_mut().unwrap().remove("mount_types");
    let legacy = serde_json::from_value(legacy).unwrap();
    let upgrade = inspection.recheck_against(&legacy);
    assert!(
        !upgrade
            .iter()
            .any(|path| path == "/" || path == "/System/Volumes/Data"),
        "boot namespace type upgrade must not rescan Data: {upgrade:?}"
    );

    assert!(
        !inspection
            .recheck_roots
            .iter()
            .any(|root| root == "/System/Volumes/Data" || root == "/"),
        "boot namespace would rescan: {:?}",
        inspection.recheck_roots
    );
}
#[test]
fn cancelled_reconciliation_retains_unvisited_coverage_and_completed_proof() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    let denied = root.join("denied");
    std::fs::create_dir_all(&denied).unwrap();
    std::fs::write(denied.join("secret.txt"), "secret").unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let denied = denied
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o000)).unwrap();
    let engine = SearchEngine::open(&temp.path().join("index/index.sqlite")).unwrap();
    engine.call(json!({"op":"watch","roots":[root]}));
    await_watching(&engine);
    stop_join(&engine);
    let (before, proof, cursor) = {
        let store = engine.index_store.lock().unwrap();
        (
            store.get("uncovered", json!([])),
            store.get("scan_resume_proof", Value::Null),
            store.get("event_id", json!(0)),
        )
    };
    assert_eq!(before, json!([denied]));
    assert!(!proof.is_null());
    engine.stop.store(false, Ordering::Relaxed);
    engine.scan_cancel.store(true, Ordering::Relaxed);
    let cancelled = engine.reconcile(
        std::slice::from_ref(&root),
        std::slice::from_ref(&root),
        cursor.as_u64().unwrap(),
        true,
        false,
    );
    let store = engine.index_store.lock().unwrap();
    let after = store.get("uncovered", json!([]));
    let proof_after = store.get("scan_resume_proof", Value::Null);
    std::fs::set_permissions(&denied, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!cancelled.unwrap());
    assert_eq!(after, before, "cancel cannot erase unvisited coverage gaps");
    assert_eq!(proof_after, proof);
    assert_eq!(store.get("event_id", json!(0)), cursor);
}
#[test]
fn checkpoint_does_not_absorb_access_changes_that_were_not_reconciled() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file.txt"), "data").unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let roots = [root.clone()];
    let engine = SearchEngine::open(&temp.path().join("index/index.sqlite")).unwrap();
    engine.call(json!({"op":"scan","roots":roots,"watch":false,"wait":true}));
    let before = scan_resume::inspect(&roots, &[]).unwrap().proof;
    // This change happened after the inspection that selected the recheck
    // scopes. The checkpoint must keep the inspected view, leaving the access
    // difference detectable on the following startup.
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = engine.checkpoint_resume_proof(&roots, &before);
    let after = scan_resume::inspect(&roots, &[]).unwrap();
    let saved = engine
        .index_store
        .lock()
        .unwrap()
        .get("scan_resume_proof", Value::Null);
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    result.unwrap();
    assert_eq!(saved, serde_json::to_value(&before).unwrap());
    assert_eq!(after.recheck_against(&before), roots);
}
#[test]
fn wrapped_history_invalidates_high_cursor_atomically_and_restarts_with_a_new_baseline() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("files");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("file.txt"), "data").unwrap();
    let root = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let db = temp.path().join("index/index.sqlite");
    let engine = SearchEngine::open(&db).unwrap();
    engine.call(json!({"op":"watch","roots":[root]}));
    await_watching(&engine);
    stop_join(&engine);
    let high = u64::MAX - 1;
    let old_proof = {
        let store = engine.index_store.lock().unwrap();
        store.set("event_id", &json!(high)).unwrap();
        store.get("scan_resume_proof", Value::Null)
    };
    let events = [
        scanner::ChangeEvent::from_flags("/outside/unrelated", high, 0),
        scanner::ChangeEvent::from_flags("/undefined", 7, file_events::EVENT_IDS_WRAPPED),
    ];
    // Inject a settings write error midway: neither proof nor cursor may be
    // partially reset. This uses only the tiny fixture database.
    engine.index_store.lock().unwrap().connection.execute_batch("CREATE TEMP TRIGGER fail_cursor_reset BEFORE UPDATE ON settings WHEN new.key='event_id' AND new.value='0' BEGIN SELECT RAISE(ABORT,'injected reset failure'); END;").unwrap();
    assert!(engine.invalidate_wrapped_history(&events).is_err());
    {
        let store = engine.index_store.lock().unwrap();
        assert_eq!(store.get("event_id", json!(0)), json!(high));
        assert_eq!(store.get("scan_resume_proof", Value::Null), old_proof);
        store
            .connection
            .execute_batch("DROP TRIGGER fail_cursor_reset")
            .unwrap();
    }
    assert!(engine.invalidate_wrapped_history(&events).unwrap());
    {
        let store = engine.index_store.lock().unwrap();
        assert_eq!(store.get("event_id", json!(1)), 0);
        assert_eq!(store.get("scan_resume_proof", Value::Null), Value::Null);
        assert_eq!(
            store.get("event_history_invalid", Value::Null),
            "event_ids_wrapped"
        );
    }
    // Reopening after invalidation models cancellation/crash before rebuilding.
    drop(engine);
    let engine = SearchEngine::open(&db).unwrap();
    let rebuilt = await_watching(&engine);
    assert_eq!(rebuilt["resume_reason"], "event_ids_wrapped");
    assert_eq!(rebuilt["resumed"], false);
    assert!(rebuilt["event_id"].as_u64().unwrap() < high);
    assert!(rebuilt["scan_processed_entries"].as_u64().unwrap() >= 2);
    assert_eq!(
        engine.call(json!({"op":"query","text":"file.txt"}))["total"],
        1
    );
    stop_join(&engine);
    let store = engine.index_store.lock().unwrap();
    assert!(!store.get("scan_resume_proof", Value::Null).is_null());
    assert_eq!(store.get("event_history_invalid", Value::Null), Value::Null);
}

#[test]
fn stop_during_alias_verification_rolls_back_the_batch_without_hiding_database_errors() {
    struct StopAtStatement {
        engine: Arc<SearchEngine>,
        statement_prefix: &'static [u8],
        fired: AtomicBool,
    }
    unsafe extern "C" fn stop_at_statement(
        event: u32,
        context: *mut c_void,
        _statement: *mut c_void,
        sql: *mut c_void,
    ) -> i32 {
        if event == rusqlite::ffi::SQLITE_TRACE_STMT && !sql.is_null() {
            // SQLite borrows the SQL string and the pinned test context for this
            // synchronous callback. Both remain alive until tracing is removed.
            let context = unsafe { &*(context.cast::<StopAtStatement>()) };
            let sql = unsafe { CStr::from_ptr(sql.cast()) };
            if sql.to_bytes().starts_with(context.statement_prefix) {
                context.fired.store(true, Ordering::Relaxed);
                // These are precisely the two tokens set by the public Stop op.
                context.engine.stop.store(true, Ordering::Relaxed);
                context.engine.scan_cancel.store(true, Ordering::Relaxed);
            }
        }
        0
    }
    for database_failure in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        std::fs::create_dir(&root).unwrap();
        let first = root.join("a.txt");
        let second = root.join("b.txt");
        std::fs::write(&first, "old").unwrap();
        std::fs::hard_link(&first, &second).unwrap();
        let root = root.canonicalize().unwrap();
        let engine = SearchEngine::open(&temp.path().join("index/index.sqlite")).unwrap();
        engine.call(json!({"op":"watch","roots":[root]}));
        await_watching(&engine);
        stop_join(&engine);
        let (cursor, proof) = {
            let store = engine.index_store.lock().unwrap();
            // Force a small, valid resume scope, so this is incremental alias
            // verification rather than a fresh baseline without known peers.
            store.set("uncovered", &json!([root])).unwrap();
            (
                store.get("event_id", json!(0)),
                store.get("scan_resume_proof", Value::Null),
            )
        };
        std::fs::write(&first, "changed after the baseline").unwrap();
        let mut trace = Box::new(StopAtStatement {
            engine: engine.clone(),
            statement_prefix: if database_failure {
                b"INSERT INTO files("
            } else {
                b"SELECT path FROM files WHERE volume_id="
            },
            fired: AtomicBool::new(false),
        });
        {
            let store = engine.index_store.lock().unwrap();
            if database_failure {
                store.connection.execute_batch("CREATE TEMP TRIGGER fail_file_update BEFORE INSERT ON files BEGIN SELECT RAISE(ABORT,'injected database write failure'); END;").unwrap();
            }
            // Test-only synchronization at the actual SQLite transaction
            // boundary avoids timing sleeps or thousands of hard-link fixtures.
            unsafe {
                assert_eq!(
                    rusqlite::ffi::sqlite3_trace_v2(
                        store.connection.handle(),
                        rusqlite::ffi::SQLITE_TRACE_STMT,
                        Some(stop_at_statement),
                        (&mut *trace as *mut StopAtStatement).cast(),
                    ),
                    rusqlite::ffi::SQLITE_OK
                );
            }
        }
        engine
            .start_worker(&json!({"roots":[root],"watch":true}), true)
            .unwrap();
        if let Some(worker) = engine.worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
        let store = engine.index_store.lock().unwrap();
        // Remove SQLite's borrowed pointer before dropping the test context.
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                store.connection.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
        assert!(trace.fired.load(Ordering::Relaxed));
        let status = engine.state.lock().unwrap().clone();
        if database_failure {
            assert_eq!(status["state"], "error", "{status}");
            assert!(status["errors"][0]
                .as_str()
                .unwrap()
                .contains("injected database write failure"));
        } else {
            assert_eq!(status["state"], "stopped", "{status}");
            assert_eq!(status["errors"], json!([]), "{status}");
        }
        assert_eq!(store.get("event_id", json!(0)), cursor);
        assert_eq!(store.get("scan_resume_proof", Value::Null), proof);
        assert_eq!(store.get("uncovered", json!([])), json!([root]));
        let stale: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM files WHERE is_dir=0 AND size=3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stale, 2,
            "the cancelled/failed batch must remain uncommitted"
        );
    }
}

#[test]
fn namespace_cleanup_preserves_explicit_descendants_and_cancelled_scopes() {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let paths = [
        "/System/Volumes/Data/.namespace-fixture",
        "/System/Volumes/Preboot",
        "/System/Volumes/Preboot/chosen",
        "/System/Volumes/Preboot/chosen/keep.txt",
        "/System/Volumes/Preboot/other",
        "/System/Volumes/Preboot/other/remove.txt",
        "/System/Volumes/VM/old.txt",
    ];
    let entries: Vec<_> = paths
        .iter()
        .map(|path| scanner::ScannedFile {
            path: (*path).into(),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        })
        .collect();
    {
        let mut store = engine.index_store.lock().unwrap();
        store.batch(&entries, 1).unwrap();
        store.set("event_id", &json!(88)).unwrap();
        store
            .set(
                "scan_resume_proof",
                &json!({"sentinel":"completed proof stays intact"}),
            )
            .unwrap();
        store
            .set(
                "uncovered",
                &json!([
                    "/System/Volumes/Preboot/other/denied",
                    "/System/Volumes/Preboot/chosen/denied"
                ]),
            )
            .unwrap();
    }
    engine.refresh(false).unwrap();
    let configured = ["/".into(), "/System/Volumes/Preboot/chosen".into()];
    engine.scan_cancel.store(true, Ordering::Relaxed);
    assert!(!engine.prune_index_namespace(&configured).unwrap());
    assert_eq!(
        engine.index_store.lock().unwrap().entries().unwrap().len(),
        paths.len()
    );
    engine.scan_cancel.store(false, Ordering::Relaxed);
    assert!(engine.prune_index_namespace(&configured).unwrap());
    let store = engine.index_store.lock().unwrap();
    let remaining: BTreeSet<_> = store
        .entries()
        .unwrap()
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(
        remaining,
        paths[..4].iter().map(|path| (*path).to_owned()).collect()
    );
    assert_eq!(store.get("event_id", json!(0)), json!(88));
    assert_eq!(
        store.get("scan_resume_proof", Value::Null),
        json!({"sentinel":"completed proof stays intact"})
    );
    assert_eq!(
        store.get("uncovered", json!([])),
        json!(["/System/Volumes/Preboot/chosen/denied"])
    );
    assert_eq!(engine.state.lock().unwrap()["namespace_pruned_entries"], 3);
    drop(store);
    assert!(engine.prune_index_namespace(&["/".into()]).unwrap());
    assert_eq!(
        engine
            .index_store
            .lock()
            .unwrap()
            .entries()
            .unwrap()
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![paths[0]]
    );
}

#[test]
fn database_namespace_pruning_never_expands_or_resolves_literal_scopes() {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let data = "/System/Volumes/Data/.obsolete-fixture";
    let keep = "/Users/example/namespace-keep.txt";
    let entries: Vec<_> = [data, keep]
        .iter()
        .map(|path| scanner::ScannedFile {
            path: (*path).into(),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            ..Default::default()
        })
        .collect();
    {
        let mut store = engine.index_store.lock().unwrap();
        store.batch(&entries, 1).unwrap();
        store.set("event_id", &json!(88)).unwrap();
        store
            .set("scan_resume_proof", &json!({"sentinel":"completed"}))
            .unwrap();
    }
    engine.refresh(false).unwrap();
    let configured = ["/System".into(), "/Users/example".into()];
    assert!(engine.prune_index_namespace(&configured).unwrap());
    {
        let store = engine.index_store.lock().unwrap();
        assert_eq!(
            store
                .entries()
                .unwrap()
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            [keep]
        );
        assert_eq!(
            engine.state.lock().unwrap()["namespace_pruned_scopes"],
            json!(["/System/Volumes/Data"])
        );
        assert_eq!(store.get("event_id", json!(0)), json!(88));
        assert_eq!(
            store.get("scan_resume_proof", Value::Null),
            json!({"sentinel":"completed"})
        );
        // This missing literal alias exists only in persisted coverage. It must
        // be removed by its database spelling, not /Users or /private/var.
        store
            .set(
                "uncovered",
                &json!([
                    "/System/Volumes/Data/Users/example/filesearch-missing-coverage",
                    "/System/Volumes/Data/private/var/filesearch-missing-coverage"
                ]),
            )
            .unwrap();
    }
    assert!(engine.prune_index_namespace(&configured).unwrap());
    let store = engine.index_store.lock().unwrap();
    assert_eq!(
        store
            .entries()
            .unwrap()
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        [keep]
    );
    assert_eq!(store.get("uncovered", json!([])), json!([]));
    assert_eq!(
        engine.state.lock().unwrap()["namespace_pruned_scopes"],
        json!([
            "/System/Volumes/Data/Users/example/filesearch-missing-coverage",
            "/System/Volumes/Data/private/var/filesearch-missing-coverage"
        ])
    );
}
