use super::*;

#[test]
fn preference_reads_and_identical_updates_do_not_wait_for_database_work() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    let expected = engine.preferences(&json!({})).unwrap();
    let store = engine.index_store.lock().unwrap();
    let changes = store.connection.total_changes();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = engine.clone();
    let worker = std::thread::spawn(move || {
        let read = reader.preferences(&json!({"action":"get"}));
        let unchanged =
            reader.preferences(&json!({"action":"set", "values":{"macros":{}, "exclusions":[]}}));
        sender.send((read, unchanged)).unwrap();
    });
    // Keep the database deliberately unavailable, as during cache serialization.
    // Release it even on failure so a regression cannot strand the test thread.
    let replies = receiver.recv_timeout(std::time::Duration::from_secs(2));
    assert_eq!(store.connection.total_changes(), changes);
    drop(store);
    worker.join().unwrap();
    let (read, unchanged) = replies.expect("preference-only requests waited for the database");
    assert_eq!(read.unwrap(), expected);
    assert_eq!(unchanged.unwrap(), expected);
}

#[test]
fn preference_batch_failure_rolls_back_sql_and_does_not_publish_partial_values() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("index.sqlite");
    let engine = SearchEngine::open(&path).unwrap();
    let before = engine.preferences(&json!({})).unwrap();
    engine.index_store.lock().unwrap().connection.execute_batch(
        "CREATE TRIGGER reject_history BEFORE INSERT ON settings WHEN NEW.key='history' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;"
    ).unwrap();
    let failed =
        engine.preferences(&json!({"set":{"bookmarks":["fixture"],"history":["fixture"]}}));
    assert!(failed.is_err());
    assert_eq!(engine.preferences(&json!({})).unwrap(), before);
    assert_eq!(
        engine
            .index_store
            .lock()
            .unwrap()
            .get("bookmarks", json!([])),
        json!([])
    );
    engine
        .index_store
        .lock()
        .unwrap()
        .connection
        .execute_batch("DROP TRIGGER reject_history")
        .unwrap();
    let committed = engine
        .preferences(&json!({"set":{"bookmarks":["fixture"],"history":["fixture"]}}))
        .unwrap();
    drop(engine);
    let reopened = SearchEngine::open(&path).unwrap();
    assert_eq!(reopened.preferences(&json!({})).unwrap(), committed);
}

#[test]
fn concurrent_disjoint_preference_updates_preserve_both_values() {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = ["bookmarks", "history"]
        .into_iter()
        .map(|key| {
            let engine = engine.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                engine
                    .preferences(&json!({"set":{key:["fixture"]}}))
                    .unwrap();
            })
        })
        .collect();
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let current = engine.preferences(&json!({})).unwrap();
    assert_eq!(current["bookmarks"], json!(["fixture"]));
    assert_eq!(current["history"], json!(["fixture"]));
}
