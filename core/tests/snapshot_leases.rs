use apfsearch_core::SearchEngine;
use serde_json::{Value, json};
use std::{fs, os::unix::fs::MetadataExt, path::Path, sync::Arc};

fn expect_success(engine: &Arc<SearchEngine>, request: Value) -> Value {
    let response = engine.call(request.clone());
    assert_eq!(response["success"], true, "{request} => {response}");
    response
}

#[test]
fn leased_pages_keep_rows_and_query_preferences_after_history_is_evicted() {
    let temp = tempfile::tempdir().unwrap();
    let files = temp.path().join("files");
    fs::create_dir(&files).unwrap();
    let files = fs::canonicalize(files).unwrap();
    for n in 0..9 {
        fs::write(
            files.join(format!("report{n}.txt")),
            format!("original-{n}"),
        )
        .unwrap();
    }
    fs::write(files.join("excluded.txt"), "excluded").unwrap();
    fs::write(files.join("alternative.pdf"), "pdf").unwrap();
    let engine = SearchEngine::open(&temp.path().join("database/index.sqlite")).unwrap();
    let scan = || {
        expect_success(
            &engine,
            json!({"op":"scan","roots":[files],"watch":false,"wait":true}),
        )
    };
    scan();
    expect_success(
        &engine,
        json!({"op":"preferences","action":"set","values":{
            "macros":{"docs":"ext:txt"},"exclusions":["name:excluded.txt"]
        }}),
    );
    let original = expect_success(&engine, json!({"op":"query","text":"docs:","limit":100}));
    assert_eq!(original["total"], 9);
    let held = expect_success(
        &engine,
        json!({"op":"retain_snapshot","generation":original["generation"]}),
    );
    let token = held["snapshot_lease"].as_str().unwrap();

    // Change both query configuration and real metadata, publishing more
    // snapshots than the normal two-generation history retains.
    expect_success(
        &engine,
        json!({"op":"preferences","action":"set","values":{
            "macros":{"docs":"ext:pdf"},"exclusions":[]
        }}),
    );
    fs::remove_file(files.join("report0.txt")).unwrap();
    fs::write(
        files.join("report1.txt"),
        "replacement has a different size",
    )
    .unwrap();
    let mut generations = Vec::new();
    for n in 0..4 {
        fs::write(files.join(format!("new{n}.txt")), "new").unwrap();
        scan();
        generations.push(
            expect_success(&engine, json!({"op":"status"}))["generation"]
                .as_u64()
                .unwrap(),
        );
    }
    assert!(generations.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        engine.call(json!({"op":"query","text":"docs:","generation":original["generation"]}))["success"],
        false,
        "Fixture must really evict the unleased generation"
    );
    let current = expect_success(&engine, json!({"op":"query","text":"docs:"}));
    assert_eq!(current["total"], 1);
    assert_eq!(current["rows"][0]["name"], "alternative.pdf");

    let mut all_rows = Vec::new();
    for offset in [0, 2, 4, 6, 8] {
        let page = expect_success(
            &engine,
            json!({"op":"query","text":"docs:","offset":offset,"limit":2,
            "snapshot_lease":token,"generation":held["generation"]}),
        );
        assert_eq!(page["generation"], held["generation"]);
        assert_eq!(page["total"], 9);
        all_rows.extend(page["rows"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(
        all_rows,
        *original["rows"].as_array().unwrap(),
        "Rows, paths, sizes, exclusions, macro expansion, and sort must remain exactly from the lease"
    );

    let mismatch = engine.call(json!({"op":"query","text":"docs:","snapshot_lease":token,
        "generation":current["generation"]}));
    assert_eq!(mismatch["success"], false);
    assert!(mismatch["error"].as_str().unwrap().contains("generation"));
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"release_snapshot","snapshot_lease":token})
        )["released"],
        true
    );
    assert_eq!(
        engine.call(json!({"op":"query","text":"docs:","snapshot_lease":token}))["success"],
        false
    );
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"release_snapshot","snapshot_lease":token})
        )["released"],
        false
    );
    assert_eq!(
        engine.call(json!({"op":"retain_snapshot","generation":original["generation"]}))["success"],
        false
    );
    expect_success(&engine, json!({"op":"stop"}));
}

#[test]
fn independently_retained_leases_release_without_affecting_other_readers() {
    let temp = tempfile::tempdir().unwrap();
    let engine = SearchEngine::open(&temp.path().join("index.sqlite")).unwrap();
    expect_success(
        &engine,
        json!({"op":"import_file_list","rows":[{"path":"/offline/one.txt","size":11}]}),
    );
    let first = expect_success(&engine, json!({"op":"retain_snapshot"}));
    let second = expect_success(&engine, json!({"op":"retain_snapshot"}));
    assert_ne!(first["snapshot_lease"], second["snapshot_lease"]);
    expect_success(
        &engine,
        json!({"op":"release_snapshot","snapshot_lease":first["snapshot_lease"]}),
    );
    let page = expect_success(
        &engine,
        json!({"op":"query","snapshot_lease":second["snapshot_lease"],"text":"file:"}),
    );
    assert_eq!(page["total"], 1);
    assert_eq!(page["rows"][0]["path"], "/offline/one.txt");
    assert_eq!(
        engine.call(json!({"op":"query","snapshot_lease":"not-a-real-lease"}))["success"],
        false
    );
    assert_eq!(
        engine.call(json!({"op":"release_snapshot"}))["success"],
        false
    );
    expect_success(
        &engine,
        json!({"op":"release_snapshot","snapshot_lease":second["snapshot_lease"]}),
    );
}

fn expected_identity(path: &Path) -> Value {
    let metadata = fs::symlink_metadata(path).unwrap();
    json!({"file_id":metadata.ino(),"device_id":metadata.dev(),"size":metadata.len(),
        "modified_ns":metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
        "changed_ns":metadata.ctime() * 1_000_000_000 + metadata.ctime_nsec()})
}

#[test]
fn extracted_content_requires_current_file_identity_and_content_leases_reject_changed_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let files = temp.path().join("files");
    fs::create_dir(&files).unwrap();
    let files = fs::canonicalize(files).unwrap();
    let path = files.join("document.txt");
    fs::write(&path, "original filesystem bytes").unwrap();
    let engine = SearchEngine::open(&temp.path().join("database/index.sqlite")).unwrap();
    let scan = || {
        expect_success(
            &engine,
            json!({"op":"scan","roots":[files],"watch":false,"wait":true}),
        )
    };
    scan();
    expect_success(
        &engine,
        json!({"op":"put_content","path":path,"text":"first_extracted_needle",
        "expected":expected_identity(&path)}),
    );
    let lease = expect_success(&engine, json!({"op":"retain_snapshot"}));
    let original = expect_success(
        &engine,
        json!({"op":"query","text":"file:","snapshot_lease":lease["snapshot_lease"]}),
    );
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"query","text":"content:first_extracted_needle",
        "snapshot_lease":lease["snapshot_lease"]})
        )["total"],
        1
    );

    let mut wrong = expected_identity(&path);
    wrong["size"] = json!(wrong["size"].as_u64().unwrap() + 1);
    let rejected = engine.call(
        json!({"op":"put_content","path":path,"text":"poison_extracted_needle","expected":wrong}),
    );
    assert_eq!(
        rejected["success"], false,
        "Mismatched metadata must reject stale extraction"
    );
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"query","text":"content:poison_extracted_needle"})
        )["total"],
        0
    );
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"query","text":"content:first_extracted_needle"})
        )["total"],
        1
    );

    fs::write(&path, "replacement filesystem bytes with different size").unwrap();
    scan();
    expect_success(
        &engine,
        json!({"op":"put_content","path":path,"text":"second_extracted_needle",
        "expected":expected_identity(&path)}),
    );
    assert_eq!(
        expect_success(
            &engine,
            json!({"op":"query","text":"content:second_extracted_needle"})
        )["total"],
        1
    );
    let old_content = engine.call(json!({"op":"query","text":"content:first_extracted_needle",
        "snapshot_lease":lease["snapshot_lease"]}));
    assert_eq!(
        old_content["success"], false,
        "Never silently combine old metadata with newly extracted content"
    );
    assert!(
        old_content["error"]
            .as_str()
            .unwrap()
            .contains("Content revision")
    );
    let old_metadata = expect_success(
        &engine,
        json!({"op":"query","text":"file:","snapshot_lease":lease["snapshot_lease"]}),
    );
    assert_eq!(
        old_metadata["rows"], original["rows"],
        "Independent metadata-only readers remain valid"
    );
    expect_success(
        &engine,
        json!({"op":"release_snapshot","snapshot_lease":lease["snapshot_lease"]}),
    );
    expect_success(&engine, json!({"op":"stop"}));
}
