use super::*;
use crate::entry_table::FileEntry;

fn fixture() -> (tempfile::TempDir, IndexStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = IndexStore::open(&directory.path().join("index.sqlite")).unwrap();
    let files: Vec<_> = (1..=3)
        .map(|id| ScannedFile {
            path: format!("/streaming/item{id}.txt"),
            name: format!("item{id}.txt"),
            size: id,
            file_id: id,
            volume_id: "fixture".into(),
            ..ScannedFile::default()
        })
        .collect();
    store.batch(&files, 1).unwrap();
    (directory, store)
}

#[test]
fn streamed_rows_keep_one_read_view_and_release_it_before_index_work() {
    let (directory, store) = fixture();
    let expected = serde_json::to_value(store.entries().unwrap()).unwrap();
    let writer = rusqlite::Connection::open(directory.path().join("index.sqlite")).unwrap();
    let mut statement = store.connection.prepare(&format!(
        "SELECT f.id,{} FROM files f LEFT JOIN content c ON f.path=c.path WHERE f.accessible=1 ORDER BY f.id",
        change_reader::COLUMNS
    )).unwrap();
    let mut rows = statement.query_map([], change_reader::decode_file).unwrap();
    let first = rows.next().unwrap().unwrap();
    writer
        .execute("UPDATE files SET size=987 WHERE id=2", [])
        .unwrap();
    let rows =
        std::iter::once(Ok(first)).chain(rows.map(|row| row.map_err(|error| error.to_string())));
    let mut released_before_postings = false;
    let snapshot = SearchSnapshot::build_rows_with_metrics(rows, 7, |phase, _| {
        // Compact blocks are prepared while the bounded row stream is open.
        // The cursor must still be released before building global postings.
        if phase == "prepare_compact_rows" {
            let (busy, log, checkpointed): (i64, i64, i64) = writer
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .unwrap();
            assert_eq!((busy, log, checkpointed), (0, 0, 0));
            released_before_postings = true;
        }
    })
    .unwrap();
    assert!(released_before_postings);
    assert_eq!(serde_json::to_value(&snapshot.entries).unwrap(), expected);
    let changes = store.connection.total_changes();
    store
        .connection
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    let current = store.snapshot(8).unwrap();
    assert_eq!(current.entries.at(1).size(), 987);
    assert_eq!(store.connection.total_changes(), changes);
}

#[test]
fn invalid_row_aborts_read_only_recovery_without_writing_metadata() {
    let (_directory, store) = fixture();
    store
        .connection
        .execute("UPDATE files SET name=CAST(x'80' AS TEXT) WHERE id=2", [])
        .unwrap();
    let changes = store.connection.total_changes();
    store
        .connection
        .execute_batch("PRAGMA query_only=ON")
        .unwrap();
    assert!(store.snapshot(9).is_err());
    assert_eq!(store.connection.total_changes(), changes);
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        3
    );
}
