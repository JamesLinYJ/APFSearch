//! Process-death tests touch only one synthetic metadata row in a temporary DB.
use apfsearch_core::entry_table::FileEntry;
use apfsearch_core::{
    index_store::{IndexStore, SearchSnapshot},
    scanner::ScannedFile,
};
use serde_json::json;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn crash_fixture_writer() {
    let Some(path) = std::env::var_os("APFSEARCH_CRASH_FIXTURE_DB") else {
        return;
    };
    let mut store = IndexStore::open(&PathBuf::from(path)).unwrap();
    if std::env::var_os("APFSEARCH_CRASH_COMMIT").is_some() {
        store
            .batch(
                &[ScannedFile {
                    path: "/crash-fixture/item.txt".into(),
                    name: "item.txt".into(),
                    extension: "txt".into(),
                    size: 999,
                    file_id: 1,
                    volume_id: "fixture".into(),
                    ..ScannedFile::default()
                }],
                1,
            )
            .unwrap();
    } else {
        store.connection.execute_batch("BEGIN IMMEDIATE; UPDATE files SET size=999 WHERE id=1; UPDATE settings SET value=CAST(value AS INTEGER)+1 WHERE key='revision'; UPDATE settings SET value='true' WHERE key='cache_dirty';").unwrap();
    }
    std::fs::write(
        std::env::var_os("APFSEARCH_CRASH_FIXTURE_READY").unwrap(),
        "ready",
    )
    .unwrap();
    loop {
        thread::park();
    }
}

#[test]
fn killed_writer_preserves_atomic_metadata_and_recoverable_cache_history() {
    for committed in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("index.sqlite");
        let ready = temporary.path().join("ready");
        let mut store = IndexStore::open(&path).unwrap();
        store
            .batch(
                &[ScannedFile {
                    path: "/crash-fixture/item.txt".into(),
                    name: "item.txt".into(),
                    extension: "txt".into(),
                    size: 1,
                    file_id: 1,
                    volume_id: "fixture".into(),
                    ..ScannedFile::default()
                }],
                1,
            )
            .unwrap();
        store.set("generation", &json!(1)).unwrap();
        store
            .cache_write(
                &SearchSnapshot::new(store.entries().unwrap(), 1),
                store.get("revision", json!(0)).as_u64().unwrap(),
            )
            .unwrap();
        drop(store);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "crash_fixture_writer", "--nocapture"])
            .env("APFSEARCH_CRASH_FIXTURE_DB", &path)
            .env("APFSEARCH_CRASH_FIXTURE_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if committed {
            command.env("APFSEARCH_CRASH_COMMIT", "1");
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            if let Some(status) = child.try_wait().unwrap() {
                panic!("fixture child exited before ready: {status}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let observed = ready.exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            observed,
            "fixture child did not enter its controlled crash window"
        );
        let store = IndexStore::open(&path).unwrap();
        let expected = if committed { 999 } else { 1 };
        assert_eq!(store.entries().unwrap()[0].size, expected);
        let (snapshot, _) = store
            .cache_read()
            .expect("journal and metadata must recover together");
        assert_eq!(snapshot.visible_entries().next().unwrap().size(), expected);
        let count: i64 = store
            .connection
            .query_row("SELECT pending_count FROM cache_journal_state", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, i64::from(committed));
        let integrity: String = store
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(integrity, "ok");
    }
}
