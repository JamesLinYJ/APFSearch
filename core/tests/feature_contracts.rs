use filesearch_core::{index_store::IndexStore, SearchEngine};
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

fn success(engine: &Arc<SearchEngine>, request: Value) -> Value {
    let reply = engine.call(request.clone());
    assert_eq!(reply["success"], true, "{request} -> {reply}");
    reply
}
#[test]
fn query_plan_status_and_directory_queries_use_the_shared_engine() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("files");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/report.txt"), "12345").unwrap();
    let root = root.canonicalize().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("database/index.sqlite")).unwrap();
    success(
        &engine,
        json!({"op":"scan","roots":[root],"watch":false,"wait":true}),
    );
    let result = success(&engine, json!({"op":"query","text":"descendant:<ext:txt>"}));
    assert_eq!(result["total"], 2);
    let directory = success(
        &engine,
        json!({"op":"directory_info","paths":[root],"generation":result["generation"]}),
    );
    assert_eq!(directory["rows"][0]["recursive_size"], 5);
    let lease = success(
        &engine,
        json!({"op":"retain_snapshot","generation":result["generation"]}),
    );
    let pinned = success(
        &engine,
        json!({"op":"query","text":"foldersize:=5","snapshot_lease":lease["snapshot_lease"]}),
    );
    assert_eq!(pinned["total"], 2);
    let status = success(&engine, json!({"op":"status"}));
    let same = success(
        &engine,
        json!({"op":"wait_status","after":status["status_revision"],"timeout_ms":0,"request_id":"observe"}),
    );
    assert_eq!(same["status_revision"], status["status_revision"]);
    let plan = success(
        &engine,
        json!({"op":"query_plan","text":"child:<author:example>"}),
    );
    assert_eq!(plan["indexed_relations_only"], true);
    assert_eq!(plan["requires_properties"], true);
    assert_eq!(plan["requires_extraction"], false);
    success(
        &engine,
        json!({"op":"release_snapshot","snapshot_lease":lease["snapshot_lease"]}),
    );
}
#[test]
fn idle_status_observations_do_not_change_database_or_cache() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("index.sqlite");
    let engine = SearchEngine::open(&path).unwrap();
    let observer = IndexStore::open(&path).unwrap();
    observer
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let db = std::fs::read(&path).unwrap();
    let wal_path = temporary.path().join("index.sqlite-wal");
    let wal = std::fs::read(&wal_path).unwrap();
    let status = success(&engine, json!({"op":"status"}));
    for _ in 0..100 {
        let same = success(
            &engine,
            json!({"op":"wait_status","after":status["status_revision"],"timeout_ms":0,"request_id":"idle"}),
        );
        assert_eq!(same["status_revision"], status["status_revision"]);
    }
    assert_eq!(std::fs::read(&path).unwrap(), db);
    assert_eq!(std::fs::read(&wal_path).unwrap(), wal);
}
#[test]
fn status_wait_can_be_cancelled_and_does_not_leave_a_request_registration() {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    let status = success(&engine, json!({"op":"status"}));
    let worker = {
        let engine = engine.clone();
        std::thread::spawn(move || {
            engine.call(json!({"op":"wait_status","after":status["status_revision"],"request_id":"pending-status","timeout_ms":30000}))
        })
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    // Repeat cancellation only in this isolated race test to cover registration
    // happening on either side of the first cancel request. Production uses a
    // single cancellation and a bounded server timeout after disconnect.
    while !worker.is_finished() && Instant::now() < deadline {
        success(
            &engine,
            json!({"op":"cancel","request_id":"pending-status"}),
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        worker.is_finished(),
        "status cancellation did not wake the waiter"
    );
    assert_eq!(worker.join().unwrap()["success"], false);
    let fresh = success(&engine, json!({"op":"status"}));
    success(
        &engine,
        json!({"op":"wait_status","after":fresh["status_revision"],"request_id":"pending-status","timeout_ms":0}),
    );
}

#[test]
fn initial_page_lease_preserves_macros_and_order_for_later_selected_pages() {
    let temporary = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temporary.path().join("index.sqlite")).unwrap();
    success(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/fixture/alpha.txt"},{"path":"/fixture/beta.pdf"}]}),
    );
    success(
        &engine,
        json!({"op":"preferences","set":{"macros":{"selected":"ext:txt"}}}),
    );
    let initial = success(
        &engine,
        json!({"op":"query","text":"selected:","retain_snapshot":true,"limit":1}),
    );
    assert!(initial["snapshot_lease"].is_string());
    success(
        &engine,
        json!({"op":"preferences","set":{"macros":{"selected":"ext:pdf"}}}),
    );
    let pinned = success(
        &engine,
        json!({"op":"query","text":"selected:","snapshot_lease":initial["snapshot_lease"],"generation":initial["generation"]}),
    );
    assert_eq!(pinned["rows"], initial["rows"]);
    let latest = success(&engine, json!({"op":"query","text":"selected:"}));
    assert_ne!(latest["rows"], pinned["rows"]);
    success(
        &engine,
        json!({"op":"release_snapshot","snapshot_lease":initial["snapshot_lease"]}),
    );
}
