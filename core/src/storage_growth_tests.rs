use super::index_store::IndexStore;
use rusqlite::Connection;

fn wal_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path.with_extension("sqlite-wal"))
        .unwrap()
        .len()
}

#[test]
fn normal_wal_reuse_reclaims_old_peak_without_rewriting_database() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let old = IndexStore::open(&path).unwrap();
    old.connection
        .pragma_update(None, "journal_size_limit", -1)
        .unwrap();
    old.connection
        .execute_batch(
            "CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES(zeroblob(20000000));",
        )
        .unwrap();
    let peak = wal_size(&path);
    assert!(peak > 16 * 1024 * 1024);
    // Keep the old connection open: this must recover an existing allocation,
    // not merely delete the WAL on the final connection's close.
    let store = IndexStore::open(&path).unwrap();
    assert_eq!(store.connection.total_changes(), 0);
    let checkpoint_pages: i64 = store
        .connection
        .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
        .unwrap();
    assert_eq!(checkpoint_pages, 1000);
    store
        .connection
        .execute("INSERT INTO payload VALUES(x'02')", [])
        .unwrap();
    assert!(wal_size(&path) <= 16 * 1024 * 1024);
    let length: i64 = store
        .connection
        .query_row("SELECT length(value) FROM payload WHERE rowid=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(length, 20_000_000);
}

#[test]
fn active_reader_keeps_required_wal_until_normal_reset_is_safe() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let store = IndexStore::open(&path).unwrap();
    store
        .connection
        .execute_batch("CREATE TABLE payload(value BLOB); INSERT INTO payload VALUES(x'01');")
        .unwrap();
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT * FROM payload;")
        .unwrap();
    store
        .connection
        .execute("INSERT INTO payload VALUES(zeroblob(20000000))", [])
        .unwrap();
    assert!(wal_size(&path) > 16 * 1024 * 1024);
    let count: i64 = reader
        .query_row("SELECT count(*) FROM payload", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    reader.execute_batch("COMMIT").unwrap();
    // Two ordinary commits allow automatic checkpointing followed by reuse.
    for _ in 0..2 {
        store
            .connection
            .execute("INSERT INTO payload VALUES(x'02')", [])
            .unwrap();
    }
    assert!(wal_size(&path) <= 16 * 1024 * 1024);
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM payload", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4);
}

#[test]
fn repeated_delete_insert_cycles_reuse_database_free_pages() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let store = IndexStore::open(&path).unwrap();
    store
        .connection
        .execute_batch("CREATE TABLE payload(value BLOB);")
        .unwrap();
    let mut previous = 0_i64;
    for cycle in 0..5 {
        store.connection.execute_batch("BEGIN; DELETE FROM payload; INSERT INTO payload VALUES(zeroblob(1000000)); COMMIT;").unwrap();
        let pages: i64 = store
            .connection
            .query_row("PRAGMA page_count", [], |r| r.get(0))
            .unwrap();
        if cycle > 0 {
            assert_eq!(pages, previous);
        }
        previous = pages;
    }
}
