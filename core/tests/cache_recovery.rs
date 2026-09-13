use apfsearch_core::{
    index_store::{IndexStore, SearchSnapshot},
    scanner::ScannedFile,
};
use serde_json::json;
fn row(name: &str, id: u64) -> ScannedFile {
    ScannedFile {
        path: format!("/cache-fixture/{name}"),
        name: name.into(),
        extension: name
            .rsplit_once('.')
            .map(|(_, value)| value.into())
            .unwrap_or_default(),
        size: id * 1024,
        file_id: id,
        volume_id: "fixture".into(),
        ..Default::default()
    }
}
fn checkpoint(store: &IndexStore, generation: u64) {
    store.set("generation", &json!(generation)).unwrap();
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let mut snapshot = SearchSnapshot::new(store.entries().unwrap(), generation);
    snapshot.content_revision = store.get("content_revision", json!(0)).as_u64().unwrap();
    store.cache_write(&snapshot, revision).unwrap();
}
fn assert_current(store: &IndexStore) {
    let (snapshot, _) = store
        .cache_read()
        .expect("valid prepared cache and complete delta should restore");
    let restored: Vec<_> = snapshot
        .visible_entries()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    let current: Vec<_> = store
        .entries()
        .unwrap()
        .iter()
        .map(|entry| serde_json::to_value(entry).unwrap())
        .collect();
    assert_eq!(restored, current);
}
#[test]
fn prepared_cache_replays_published_changes_without_rewriting_cache() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("index.sqlite");
    let mut store = IndexStore::open(&database).unwrap();
    store
        .batch(&[row("first.txt", 1), row("second.pdf", 2)], 1)
        .unwrap();
    checkpoint(&store, 1);
    let before = std::fs::metadata(&store.cache_path)
        .unwrap()
        .modified()
        .unwrap();
    store.batch(&[row("third.pdf", 3)], 2).unwrap();
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    store.clear_changes(revision).unwrap();
    store.set("generation", &json!(2)).unwrap();
    assert!(store.cache_is_dirty());
    assert_current(&store);
    assert_eq!(
        std::fs::metadata(&store.cache_path)
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    drop(store);
    assert_current(&IndexStore::open(&database).unwrap());
}
#[test]
fn cache_journal_tracks_repeated_updates_after_checkpoint_and_content_changes() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&directory.path().join("index.sqlite")).unwrap();
    store.batch(&[row("first.txt", 1)], 1).unwrap();
    checkpoint(&store, 1);
    // snapshot_changes still contains this id: cache tracking must not depend on
    // inserting a previously absent row in the separate publication journal.
    let mut changed = row("first.txt", 1);
    changed.size = 9000;
    store.batch(&[changed], 2).unwrap();
    store
        .put_content("/cache-fixture/first.txt", "text", &json!({"width":100}))
        .unwrap();
    assert_current(&store);
    checkpoint(&store, 2);
    let mut changed = row("first.txt", 1);
    changed.size = 12000;
    store.batch(&[changed], 3).unwrap();
    assert_current(&store);
}
#[test]
fn cache_journal_has_bounded_writes_and_rejects_overflow_or_missing_baseline() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&directory.path().join("index.sqlite")).unwrap();
    // No valid prepared cache exists yet: a first build does not also populate
    // a second per-file journal.
    store.batch(&[row("first.txt", 1)], 1).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM cache_changes", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    checkpoint(&store, 1);
    let changed: Vec<_> = (2..=2003)
        .map(|id| row(&format!("file{id}.txt"), id))
        .collect();
    store.batch(&changed, 2).unwrap();
    assert_eq!(
        store.get("cache_journal_overflow", json!(false)),
        json!(false)
    );
    assert_current(&store);
    // Fill only disposable tombstone IDs, not a large filesystem or metadata fixture.
    // Keep positive IDs free so actual file triggers exercise the overflow transition.
    let pending: i64 = store
        .connection
        .query_row("SELECT count(*) FROM cache_changes", [], |r| r.get(0))
        .unwrap();
    let transaction = store.connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO cache_changes VALUES(?1)")
            .unwrap();
        for id in 1..=65_536 - pending {
            insert.execute([-id]).unwrap();
        }
    }
    transaction.commit().unwrap();
    assert_eq!(
        store.get("cache_journal_overflow", json!(false)),
        json!(false)
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT pending_count FROM cache_journal_state", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        65_536
    );
    store
        .batch(
            &[
                row("overflow.txt", 99_999),
                row("after-overflow.txt", 100_000),
            ],
            2,
        )
        .unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT pending_count FROM cache_journal_state", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    assert_eq!(
        store.get("cache_journal_overflow", json!(false)),
        json!(true)
    );
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM cache_changes", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(store.cache_read().is_none());
    checkpoint(&store, 2);
    assert_current(&store);
    store.set("cache_base_revision", &json!(u64::MAX)).unwrap();
    assert!(store.cache_read().is_none());
}

#[test]
fn clean_cache_damage_falls_back_marks_dirty_and_repairs_without_metadata_changes() {
    use apfsearch_core::SearchEngine;
    for remove_cache in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("files");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("report.txt"), "fixture").unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("database/index.sqlite");
        let engine = SearchEngine::open(&database).unwrap();
        let scan = || json!({"op":"scan","roots":[root],"watch":false,"wait":true});
        assert_eq!(engine.call(scan())["success"], true);
        let expected = engine.call(json!({"op":"query","text":"report"}))["rows"].clone();
        let store = IndexStore::open(&database).unwrap();
        store
            .set("bookmarks", &json!([{"name":"saved","query":"report"}]))
            .unwrap();
        assert!(!store.cache_is_dirty());
        let revision = store.get("revision", json!(0));
        if remove_cache {
            std::fs::remove_file(&store.cache_path).unwrap();
        } else {
            let mut bytes = std::fs::read(&store.cache_path).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&store.cache_path, bytes).unwrap();
        }
        assert!(store.cache_read().is_none());
        drop(engine);
        let reopened = SearchEngine::open(&database).unwrap();
        assert_eq!(
            reopened.call(json!({"op":"query","text":"report"}))["rows"],
            expected
        );
        // The rejected cache is marked dirty, then repaired in the background
        // even without a rescan or unrelated metadata change.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while store.cache_read().is_none() && std::time::Instant::now() < deadline {
            assert!(store.cache_is_dirty());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            store.get("revision", json!(0)),
            revision,
            "Repair must not require an unrelated file change"
        );
        assert!(!store.cache_is_dirty());
        assert_current(&store);
        assert_eq!(
            store.get("bookmarks", json!(null)),
            json!([{"name":"saved","query":"report"}])
        );
    }
}

#[test]
fn cache_write_failure_keeps_committed_results_available_and_reports_cache_error() {
    use apfsearch_core::SearchEngine;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("files");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("new-result.txt"), "fixture").unwrap();
    let root = root.canonicalize().unwrap();
    let database = directory.path().join("database/index.sqlite");
    let engine = SearchEngine::open(&database).unwrap();
    let store = IndexStore::open(&database).unwrap();
    // A directory in place of the cache deterministically fails atomic rename.
    std::fs::create_dir(&store.cache_path).unwrap();
    let scan = || json!({"op":"scan","roots":[root],"watch":false,"wait":true});
    assert_eq!(engine.call(scan())["success"], true);
    let response = engine.call(json!({"op":"query","text":"new-result"}));
    assert_eq!(
        response["total"], 1,
        "A disposable cache must not block snapshot publication"
    );
    assert_eq!(store.entries().unwrap().len(), 2);
    let failed = engine.call(json!({"op":"status"}));
    assert!(failed["cache_error"]
        .as_str()
        .is_some_and(|text| !text.is_empty()));
    assert_ne!(failed["state"], "error");
    assert!(store.cache_is_dirty());
    std::fs::remove_dir(&store.cache_path).unwrap();
    assert_eq!(engine.call(scan())["success"], true);
    assert!(engine.call(json!({"op":"status"}))["cache_error"].is_null());
    assert!(!store.cache_is_dirty());
    assert_current(&store);
}

#[test]
fn opening_current_schema_does_not_write_database_or_wal() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("index.sqlite");
    let observer = IndexStore::open(&database).unwrap();
    assert_eq!(
        observer
            .connection
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    observer
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let wal = directory.path().join("index.sqlite-wal");
    let database_before = std::fs::read(&database).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    for _ in 0..3 {
        drop(IndexStore::open(&database).unwrap());
    }
    assert_eq!(std::fs::read(&database).unwrap(), database_before);
    assert_eq!(
        std::fs::read(&wal).unwrap(),
        wal_before,
        "An unchanged open must not rewrite schema or journal settings"
    );
}

#[test]
fn unsupported_schema_preserves_metadata_content_and_preferences() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("index.sqlite");
    let mut observer = IndexStore::open(&database).unwrap();
    observer.batch(&[row("preserved.txt", 10)], 1).unwrap();
    observer
        .put_content(
            "/cache-fixture/preserved.txt",
            "preserved body",
            &json!({"width":123}),
        )
        .unwrap();
    let preferences = json!([{"name":"saved", "query":"preserved"}]);
    observer.set("bookmarks", &preferences).unwrap();
    observer
        .connection
        .execute_batch("PRAGMA user_version=2; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let wal = directory.path().join("index.sqlite-wal");
    let before = std::fs::read(&database).unwrap();
    let before_wal = std::fs::read(&wal).unwrap();
    assert!(IndexStore::open(&database).is_err());
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert_eq!(std::fs::read(&wal).unwrap(), before_wal);
    assert_eq!(observer.get("bookmarks", json!(null)), preferences);
    assert_eq!(
        observer
            .content_for("/cache-fixture/preserved.txt")
            .unwrap()
            .as_deref(),
        Some("preserved body")
    );
}

#[test]
fn unknown_schema_and_invalid_database_fail_without_replacing_user_data() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("index.sqlite");
    let observer = IndexStore::open(&database).unwrap();
    observer.set("bookmarks", &json!(["preserved"])).unwrap();
    observer
        .connection
        .execute_batch("PRAGMA user_version=999; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let before = std::fs::read(&database).unwrap();
    let error = match IndexStore::open(&database) {
        Ok(_) => panic!("An unknown schema must not be silently rewritten"),
        Err(error) => error,
    };
    assert!(error.contains("schema 999"));
    assert_eq!(std::fs::read(&database).unwrap(), before);
    assert_eq!(observer.get("bookmarks", json!(null)), json!(["preserved"]));
    let malformed = directory.path().join("malformed.sqlite");
    let bytes = b"This is not a SQLite database; preserve it for diagnosis.";
    std::fs::write(&malformed, bytes).unwrap();
    assert!(IndexStore::open(&malformed).is_err());
    assert_eq!(std::fs::read(malformed).unwrap(), bytes);
}

#[cfg(unix)]
#[test]
fn denied_index_open_leaves_existing_database_untouched() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        return; // Unix mode bits cannot test denied access for root.
    }
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("readonly");
    std::fs::create_dir(&parent).unwrap();
    let database = parent.join("index.sqlite");
    let observer = IndexStore::open(&database).unwrap();
    observer.set("bookmarks", &json!(["preserved"])).unwrap();
    observer
        .connection
        .execute_batch("PRAGMA user_version=2; PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(observer);
    let before = std::fs::read(&database).unwrap();
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
    let refused = IndexStore::open(&database).is_err();
    let after = std::fs::read(&database).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        refused,
        "A denied open must report failure instead of fabricating an empty index"
    );
    assert_eq!(after, before);
}

#[test]
fn metadata_commit_during_cache_write_retains_the_new_delta() {
    use std::{
        sync::mpsc,
        time::{Duration, Instant},
    };
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("index.sqlite");
    let mut store = IndexStore::open(&database).unwrap();
    let rows: Vec<_> = (1..=20_000)
        .map(|id| row(&format!("file{id}.txt"), id))
        .collect();
    store.batch(&rows, 1).unwrap();
    checkpoint(&store, 1);
    let revision = store.get("revision", json!(0)).as_u64().unwrap();
    let snapshot = SearchSnapshot::new(store.entries().unwrap(), 1);
    let writer_database = database.clone();
    let (done_sender, done_receiver) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        let writer_store = IndexStore::open(&writer_database).unwrap();
        writer_store.cache_write(&snapshot, revision).unwrap();
        done_sender.send(()).unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|extension| extension == "tmp")
        }) {
            break;
        }
        assert!(
            done_receiver.try_recv().is_err(),
            "The writer must still be serializing when the concurrent commit begins"
        );
        assert!(
            Instant::now() < deadline,
            "Cache writer did not create its atomic-rename temporary file"
        );
        std::thread::yield_now();
    }
    let mut changed = row("file1.txt", 1);
    changed.size = 999_999;
    store.batch(&[changed], 2).unwrap();
    store.set("generation", &json!(2)).unwrap();
    writer.join().unwrap();
    assert!(
        store.cache_is_dirty(),
        "An old cache writer must not checkpoint a newer SQLite revision"
    );
    assert_current(&store);
    assert_eq!(
        store
            .cache_read()
            .unwrap()
            .0
            .visible_entries()
            .find(|entry| entry.name == "file1.txt")
            .unwrap()
            .size,
        999_999
    );
}
