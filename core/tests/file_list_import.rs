use apfsearch_core::SearchEngine;
use serde_json::{Value, json};
use std::sync::Arc;
fn engine() -> (tempfile::TempDir, Arc<SearchEngine>) {
    let directory = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    (directory, engine)
}
fn ok(engine: &Arc<SearchEngine>, request: Value) -> Value {
    let result = engine.call(request);
    assert_eq!(result["success"], true, "{result}");
    result
}
fn query(engine: &Arc<SearchEngine>) -> Value {
    ok(engine, json!({"op":"query","text":"","limit":10000}))
}
#[test]
fn staged_import_publishes_once_and_preserves_batched_metadata() {
    let (directory, engine) = engine();
    ok(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/old.txt"}]}),
    );
    let before = query(&engine);
    ok(
        &engine,
        json!({"op":"begin_file_list_import","request_id":"batch"}),
    );
    for batch in 0..3 {
        let rows: Vec<Value> = (0..3000).map(|row| {
            let index = batch * 3000 + row;
            json!({"path":format!("C:\\offline\\文件{index:05}.TXT"),"size":index,"modified":1720000000_i64})
        }).collect();
        ok(
            &engine,
            json!({"op":"append_file_list_import","request_id":"batch","rows":rows}),
        );
        ok(&engine, json!({"op":"publish_content"}));
        let pending = query(&engine);
        assert_eq!(pending["generation"], before["generation"]);
        assert_eq!(pending["rows"][0]["path"], "/old.txt");
    }
    let done = ok(
        &engine,
        json!({"op":"finish_file_list_import","request_id":"batch"}),
    );
    assert_eq!(done["imported"], 9000);
    let published = query(&engine);
    assert_eq!(published["total"], 9000);
    assert_eq!(
        published["generation"].as_u64().unwrap(),
        before["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(published["rows"][8999]["size"], 8999);
    assert_eq!(published["rows"][8999]["name"], "文件08999.TXT");
    assert_eq!(published["rows"][8999]["extension"], "txt");
    let reopened = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    assert_eq!(query(&reopened)["total"], 9000);
    assert_eq!(query(&reopened)["offline"], true);
}
#[test]
fn malformed_and_oversized_imports_leave_existing_database_unchanged() {
    let (directory, engine) = engine();
    ok(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/old.txt"}]}),
    );
    let before = query(&engine);
    for rows in [
        json!([{"path":"/accepted.txt"},{}]),
        json!([{"path":"x".repeat(65537)}]),
    ] {
        assert_eq!(
            engine.call(json!({"op":"import_file_list","rows":rows}))["success"],
            false
        );
    }
    ok(
        &engine,
        json!({"op":"begin_file_list_import","request_id":"failure"}),
    );
    ok(
        &engine,
        json!({"op":"append_file_list_import","request_id":"failure","rows":[{"path":"/partial.txt"}]}),
    );
    let bad =
        engine.call(json!({"op":"append_file_list_import","request_id":"failure","rows":[{}]}));
    assert_eq!(bad["success"], false);
    assert_eq!(
        engine.call(json!({"op":"finish_file_list_import","request_id":"failure"}))["success"],
        false
    );
    ok(&engine, json!({"op":"publish_content"}));
    assert_eq!(query(&engine)["rows"], before["rows"]);
    assert_eq!(query(&engine)["generation"], before["generation"]);
    let reopened = SearchEngine::open(&directory.path().join("index.sqlite")).unwrap();
    assert_eq!(query(&reopened)["rows"], before["rows"]);
}
#[test]
fn cancellation_aborts_staging_and_releases_session() {
    let (_directory, engine) = engine();
    let before = query(&engine);
    ok(
        &engine,
        json!({"op":"begin_file_list_import","request_id":"cancel-import"}),
    );
    ok(
        &engine,
        json!({"op":"append_file_list_import","request_id":"cancel-import","rows":[{"path":"/partial.txt"}]}),
    );
    ok(&engine, json!({"op":"cancel","request_id":"cancel-import"}));
    assert_eq!(
        engine.call(json!({"op":"finish_file_list_import","request_id":"cancel-import"}))["success"],
        false
    );
    assert_eq!(query(&engine)["generation"], before["generation"]);
    assert_eq!(query(&engine)["total"], 0);
    ok(
        &engine,
        json!({"op":"begin_file_list_import","request_id":"next"}),
    );
    ok(
        &engine,
        json!({"op":"finish_file_list_import","request_id":"next"}),
    );
    assert_eq!(query(&engine)["offline"], true);
}
#[test]
fn batch_row_and_byte_limits_abort_without_partial_publication() {
    let (_directory, engine) = engine();
    for (id, rows) in [
        ("rows", vec![json!({"path":"/small.txt"}); 4097]),
        (
            "bytes",
            (0..20)
                .map(|index| json!({"path":format!("/{index}/{}", "p".repeat(60000)),"name":"n"}))
                .collect(),
        ),
    ] {
        ok(
            &engine,
            json!({"op":"begin_file_list_import","request_id":id}),
        );
        let result =
            engine.call(json!({"op":"append_file_list_import","request_id":id,"rows":rows}));
        assert_eq!(result["success"], false);
        assert_eq!(
            result["error"],
            "The file list exceeds the supported import limits."
        );
        assert_eq!(query(&engine)["total"], 0);
    }
    ok(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/valid.txt"}]}),
    );
    assert_eq!(query(&engine)["total"], 1);
}
#[test]
fn direct_import_is_atomic_across_multiple_conversion_batches_and_accepts_empty_replacement() {
    let (_directory, engine) = engine();
    let rows: Vec<Value> = (0..9001)
        .map(|row| json!({"path":format!("/direct/{row:05}.txt")}))
        .collect();
    ok(&engine, json!({"op":"import_file_list","rows":rows}));
    assert_eq!(query(&engine)["total"], 9001);
    ok(&engine, json!({"op":"import_file_list","rows":[]}));
    assert_eq!(query(&engine)["total"], 0);
}
