use super::*;

fn insert(store: &mut IndexStore, paths: &[&str]) {
    let entries: Vec<_> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| ScannedFile {
            path: (*path).into(),
            name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            file_id: index as u64 + 10,
            volume_id: "fixture-volume".into(),
            size: 3,
            link_count: Some(1),
            ..Default::default()
        })
        .collect();
    store.batch(&entries, 1).unwrap();
}

fn checkpoint(store: &IndexStore) {
    store.set("generation", &json!(1)).unwrap();
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let snapshot = SearchSnapshot::new(store.entries().unwrap(), 1);
    store.cache_write(&snapshot, revision).unwrap();
    store.clear_changes(revision).unwrap();
}

fn state(store: &IndexStore) -> Value {
    let rows = |sql: &str| -> Vec<(String, String)> {
        store
            .connection
            .prepare(sql)
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    json!({
        "files":rows("SELECT path,CAST(id AS TEXT) FROM files ORDER BY path"),
        "content":rows("SELECT path,body FROM content ORDER BY path"),
        "fts":rows("SELECT path,body FROM content_fts ORDER BY path"),
        "settings":rows("SELECT key,value FROM settings ORDER BY key"),
        "snapshot_changes":rows("SELECT CAST(id AS TEXT),'' FROM snapshot_changes ORDER BY id"),
        "cache_changes":rows("SELECT CAST(id AS TEXT),'' FROM cache_changes ORDER BY id")
    })
}

#[test]
fn indexed_child_scopes_seek_past_subtrees_without_skipping_prefix_neighbors() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&temp.path().join("index.sqlite")).unwrap();
    insert(
        &mut store,
        &[
            "/namespace/Data",
            "/namespace/Data/deep/leaf",
            "/namespace/Update",
            "/namespace/Update/leaf",
            "/namespace/Update/z/deep/leaf",
            "/namespace/Update-old",
            "/namespace/Update-old/leaf",
            "/namespace/Update.txt",
            "/namespace/Update0",
            "/namespace/Update0/leaf",
            "/namespace/%_中文",
            "/namespace/%_中文/leaf",
            "/namespace/MissingRoot/leaf",
            "/namespace-other/keep",
            "/namespace0/keep",
        ],
    );
    let before = store.connection.total_changes();
    assert_eq!(
        store.indexed_child_scopes("/namespace/").unwrap(),
        [
            "/namespace/%_中文",
            "/namespace/Data",
            "/namespace/MissingRoot",
            "/namespace/Update",
            "/namespace/Update-old",
            "/namespace/Update.txt",
            "/namespace/Update0",
        ]
    );
    assert_eq!(store.connection.total_changes(), before);
}

#[test]
fn namespace_prune_is_bounded_atomic_and_tracks_deleted_content_and_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let real_directory = temp.path().join("aux%_中文");
    std::fs::create_dir(&real_directory).unwrap();
    let real_file = real_directory.join("keep-on-disk.txt");
    std::fs::write(&real_file, "real file").unwrap();
    let root = real_directory.to_str().unwrap();
    let child = real_file.to_str().unwrap();
    let neighbor = format!("{root}-neighbor");
    let mut store = IndexStore::open(&temp.path().join("index.sqlite")).unwrap();
    insert(
        &mut store,
        &[root, child, &neighbor, "/System/Volumes/Data/keep"],
    );
    store
        .put_content(child, "extracted body", &json!({}))
        .unwrap();
    store
        .put_content(&neighbor, "neighbor body", &json!({}))
        .unwrap();
    store.set("uncovered", &json!([child, neighbor])).unwrap();
    checkpoint(&store);
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let removed_ids: Vec<i64> = store
        .connection
        .prepare("SELECT id FROM files WHERE path=?1 OR path=?2 ORDER BY id")
        .unwrap()
        .query_map(params![root, child], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        store
            .prune_namespace_scopes(&[format!("{root}/"), root.into()], &AtomicBool::new(false))
            .unwrap(),
        Some(2)
    );
    assert_eq!(store.get("revision", json!(0)), json!(revision + 1));
    assert_eq!(store.get("cache_dirty", json!(false)), true);
    for table in ["snapshot_changes", "cache_changes"] {
        let ids: Vec<i64> = store
            .connection
            .prepare(&format!("SELECT id FROM {table} ORDER BY id"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, removed_ids);
    }
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM content WHERE path=?1",
                [child],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM content_fts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(std::fs::read(&real_file).unwrap(), b"real file");
    assert_eq!(store.entries().unwrap().len(), 2);
    assert_eq!(store.get("uncovered", json!([])), json!([neighbor]));
    let before = state(&store);
    let writes = store.connection.total_changes();
    assert_eq!(
        store
            .prune_namespace_scopes(&[root.into()], &AtomicBool::new(false))
            .unwrap(),
        Some(0)
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        store.connection.total_changes(),
        writes,
        "no-op pruning must not write settings or journals"
    );
}

#[test]
fn cancelled_or_failed_namespace_prune_rolls_back_every_scope_and_journal() {
    unsafe extern "C" fn cancel_at_delete(
        event: u32,
        context: *mut std::ffi::c_void,
        _: *mut std::ffi::c_void,
        sql: *mut std::ffi::c_void,
    ) -> i32 {
        if event == rusqlite::ffi::SQLITE_TRACE_STMT && !sql.is_null() {
            // The flag and SQLite's borrowed statement text stay alive throughout
            // this synchronous test-only trace; tracing is removed before return.
            let text = unsafe { std::ffi::CStr::from_ptr(sql.cast()) };
            if text.to_bytes().starts_with(b"DELETE FROM files") {
                unsafe { &*context.cast::<AtomicBool>() }.store(true, Ordering::Relaxed);
            }
        }
        0
    }
    for fail in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut store = IndexStore::open(&temp.path().join("index.sqlite")).unwrap();
        insert(&mut store, &["/scope/a", "/scope/b", "/keep"]);
        store.put_content("/scope/a", "body", &json!({})).unwrap();
        store
            .set("uncovered", &json!(["/scope/a", "/keep"]))
            .unwrap();
        checkpoint(&store);
        let cancelled = AtomicBool::new(false);
        let before = state(&store);
        if fail {
            store.connection.execute_batch("CREATE TEMP TRIGGER fail_prune BEFORE DELETE ON files WHEN old.path='/scope/b' BEGIN SELECT RAISE(ABORT,'injected namespace prune failure'); END;").unwrap();
        } else {
            unsafe {
                rusqlite::ffi::sqlite3_trace_v2(
                    store.connection.handle(),
                    rusqlite::ffi::SQLITE_TRACE_STMT,
                    Some(cancel_at_delete),
                    (&cancelled as *const AtomicBool).cast_mut().cast(),
                );
            }
        }
        let result =
            store.prune_namespace_scopes(&["/scope/a".into(), "/scope/b".into()], &cancelled);
        unsafe {
            rusqlite::ffi::sqlite3_trace_v2(
                store.connection.handle(),
                0,
                None,
                std::ptr::null_mut(),
            );
        }
        if fail {
            assert!(
                result
                    .unwrap_err()
                    .contains("injected namespace prune failure")
            );
        } else {
            assert_eq!(result.unwrap(), None);
            assert!(cancelled.load(Ordering::Relaxed));
            assert_eq!(
                store
                    .prune_namespace_scopes(&["/scope".into()], &cancelled)
                    .unwrap(),
                None
            );
        }
        assert_eq!(state(&store), before);
    }
}

#[test]
fn namespace_prune_removes_obsolete_coverage_without_revising_unchanged_search_rows() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&temp.path().join("index.sqlite")).unwrap();
    insert(&mut store, &["/keep"]);
    store
        .set("uncovered", &json!(["/gone/denied", "/gone-other/denied"]))
        .unwrap();
    checkpoint(&store);
    let revision = store.get("revision", json!(0));
    assert_eq!(
        store
            .prune_namespace_scopes(&["/gone".into()], &AtomicBool::new(false))
            .unwrap(),
        Some(0)
    );
    assert_eq!(
        store.get("uncovered", json!([])),
        json!(["/gone-other/denied"])
    );
    assert_eq!(store.get("revision", json!(0)), revision);
    assert!(!store.cache_is_dirty());
}
