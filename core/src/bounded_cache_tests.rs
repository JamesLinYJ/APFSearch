use super::*;
use std::sync::atomic::AtomicBool;

fn fixture(count: usize) -> (tempfile::TempDir, IndexStore, Vec<ScannedFile>) {
    let temporary = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&temporary.path().join("index.sqlite")).unwrap();
    let files: Vec<_> = (0..count).map(|index| ScannedFile {
        path: format!("/delta-fixture/item{index}.txt"),
        name: format!("item{index}.txt"),
        extension: "txt".into(),
        size: index as u64 + 1,
        file_id: index as u64 + 100,
        parent_id: 1,
        volume_id: "delta-fixture".into(),
        ..ScannedFile::default()
    }).collect();
    store.batch(&files, 1).unwrap();
    store.set("generation", &json!(1)).unwrap();
    let base = SearchSnapshot::new(store.entries().unwrap(), 1);
    store.cache_write(&base, revision(&store)).unwrap();
    (temporary, store, files)
}

fn revision(store: &IndexStore) -> u64 {
    store.get("revision", json!(0)).as_u64().unwrap()
}

fn count(store: &IndexStore) -> i64 {
    store.connection.query_row("SELECT pending_count FROM cache_journal_state", [], |row| row.get(0)).unwrap()
}

fn writes(store: &IndexStore) -> i64 {
    store.connection.query_row("SELECT total_changes()", [], |row| row.get(0)).unwrap()
}

fn rows(snapshot: &SearchSnapshot) -> Value {
    let mut entries: Vec<_> = snapshot.visible_entries().collect();
    entries.sort_unstable_by_key(|file| file.id);
    serde_json::to_value(entries).unwrap()
}

fn paths(snapshot: &SearchSnapshot, order: &[u32]) -> Vec<String> {
    order.iter().map(|slot| snapshot.entries[*slot as usize].path.clone()).collect()
}

#[test]
fn replay_more_than_two_thousand_changes_is_read_only_and_matches_sql() {
    // Only metadata rows are synthetic: no corresponding filesystem tree is made.
    let (_temporary, mut store, mut files) = fixture(3_005);
    let old_bytes = std::fs::read(&store.cache_path).unwrap();
    for file in &mut files[..2_501] {
        file.size += 10_000;
        file.modified_ns += 1;
    }
    store.batch(&files[..2_501], 1).unwrap();
    store.finish_observed(&[], &[files[3_002].path.clone()], &RoaringTreemap::new(), 7).unwrap();
    store.prune_namespace_scopes(&[files[3_003].path.clone()], &AtomicBool::new(false)).unwrap();
    let aliases: Vec<_> = ["alias-one.txt", "alias-two.txt"].into_iter().map(|name| ScannedFile {
        path: format!("/delta-fixture/{name}"),
        name: name.into(),
        file_id: 999_999,
        ..files[0].clone()
    }).collect();
    store.batch(&aliases, 1).unwrap();
    store.put_content(&files[1].path, "fixture text", &json!({"author":"Fixture Author"})).unwrap();
    store.set("generation", &json!(2)).unwrap();
    // Publishing may consume the ordinary journal, never the cache journal.
    store.clear_changes(revision(&store)).unwrap();
    assert!(count(&store) > 2_000);
    let before = writes(&store);
    let (restored, generation) = store.cache_read().expect("bounded cache replay must not fall back to SQL");
    assert_eq!(generation, 2);
    assert_eq!(writes(&store), before, "cache replay wrote SQLite rows");
    assert_eq!(std::fs::read(&store.cache_path).unwrap(), old_bytes, "cache replay rewrote the binary cache");
    let expected = SearchSnapshot::new(store.entries().unwrap(), 2);
    assert_eq!(rows(&restored), rows(&expected));
    assert_eq!(paths(&restored, &restored.name_order), paths(&expected, &expected.name_order));
    assert_eq!(paths(&restored, &restored.path_order), paths(&expected, &expected.path_order));
    assert_eq!(restored.visible_entries().filter(|file| file.file_id == 999_999).count(), 2);
    assert_eq!(restored.content_revision, store.get("content_revision", json!(0)).as_u64().unwrap());
    let before = writes(&store);
    assert_eq!(store.observe_batch(&files[..2_501], None, None, None).unwrap(), 0);
    assert_eq!(writes(&store), before, "unchanged observations wrote SQLite rows");
    store.cache_write(&restored, revision(&store)).unwrap();
    assert_eq!(count(&store), 0);
    assert!(store.cache_read().is_some());
}

#[test]
fn schema_three_migration_preserves_both_valid_and_overflowed_histories() {
    for overflowed in [false, true] {
        let (temporary, mut store, mut files) = fixture(3);
        for file in &mut files[..2] { file.size += 10; }
        store.batch(&files[..2], 1).unwrap();
        store.set("cache_journal_overflow", &json!(overflowed)).unwrap();
        // Recreate V3's journal schema without altering its cache or current rows.
        store.connection.execute_batch(
            "DROP TRIGGER cache_change_limit;
             DROP TRIGGER cache_change_count_insert;
             DROP TRIGGER cache_change_count_delete;
             DROP TABLE cache_journal_state;
             CREATE TRIGGER cache_change_limit AFTER INSERT ON cache_changes
                 WHEN (SELECT count(*) FROM cache_changes)>2000 BEGIN
                 INSERT INTO settings VALUES('cache_journal_overflow','true') ON CONFLICT(key) DO UPDATE SET value='true';
                 DELETE FROM cache_changes;
             END;
             PRAGMA user_version=3;"
        ).unwrap();
        drop(store);
        let store = IndexStore::open(&temporary.path().join("index.sqlite")).unwrap();
        assert_eq!(count(&store), 2);
        assert_eq!(store.get("cache_journal_overflow", json!(false)), json!(overflowed));
        if overflowed {
            assert!(store.cache_read().is_none(), "migration must not bless an incomplete history");
        } else {
            let (restored, _) = store.cache_read().unwrap();
            assert_eq!(rows(&restored), rows(&SearchSnapshot::new(store.entries().unwrap(), 1)));
        }
        drop(store);
        let reopened = IndexStore::open(&temporary.path().join("index.sqlite")).unwrap();
        assert_eq!(writes(&reopened), 0, "an ordinary open must not repeat the schema migration");
    }
}

#[test]
fn metadata_rollback_restores_journal_and_counter_together() {
    let (_temporary, store, _files) = fixture(3);
    let before = store.entries().unwrap()[0].size;
    store.connection.execute_batch("BEGIN; UPDATE files SET size=size+1 WHERE id=1;").unwrap();
    assert_eq!(count(&store), 1);
    store.connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&store), 0);
    assert_eq!(store.entries().unwrap()[0].size, before);
    assert!(store.cache_read().is_some());
}

#[test]
fn bounded_reader_never_returns_a_truncated_delta() {
    let (_temporary, mut store, mut files) = fixture(3);
    for file in &mut files { file.size += 1; }
    store.batch(&files, 1).unwrap();
    assert!(change_reader::read(&store.connection, change_reader::Journal::Cache, 2).unwrap().is_none());
    let changes = change_reader::read(&store.connection, change_reader::Journal::Cache, 3).unwrap().unwrap();
    assert_eq!(changes.len(), 3);
    assert!(changes.iter().all(|(_, file)| file.is_some()));
}
